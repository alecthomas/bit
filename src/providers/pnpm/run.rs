use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde::{Deserialize, Serialize};

use crate::file_tracker::FileTracker;
use crate::output::BlockWriter;
use crate::provider::{
    Ambient, ApplyResult, Block, BoxError, CachePolicy, Changes, PlanAction, PlanResult, Resource, ResourceKind,
};
use crate::sha256::SHA256;

use super::{run_pnpm, workspace, workspace_dir};

/// Build a list of pnpm command arguments from resource inputs.
pub(super) fn pnpm_args(script: &str, package: Option<&str>, script_args: &[String]) -> Vec<String> {
    let mut args = Vec::new();
    if let Some(pkg) = package {
        args.push("--filter".to_owned());
        args.push(pkg.to_owned());
    }
    args.push("run".to_owned());
    args.push(script.to_owned());
    if !script_args.is_empty() {
        args.push("--".to_owned());
        args.extend(script_args.iter().cloned());
    }
    args
}

/// Render a shell-ish description of the `pnpm` invocation (for plan output).
pub(super) fn describe(script: &str, package: Option<&str>, script_args: &[String]) -> String {
    crate::providers::display_command("pnpm", &pnpm_args(script, package, script_args))
}

/// Resolve input files for a `pnpm.run` or `pnpm.test` block: auto-scan the
/// source trees (recursively, excluding common build directories and
/// declared outputs) of the projects whose script runs and of their local
/// dependencies, plus the workspace lockfile.
///
/// `outputs` and `extra_globs` are relative to the block's `dir`.
pub(super) fn resolve_inputs<T>(
    block: &Block<T>,
    package: Option<&str>,
    outputs: &[String],
    extra_globs: &[String],
    tracker: &FileTracker,
) -> Result<BTreeMap<String, SHA256>, BoxError> {
    let ws = workspace::workspace(tracker, Path::new(workspace_dir(block)))?;
    let mut files = BTreeMap::new();

    if let Some(lf) = &ws.lockfile {
        files.insert(lf.to_string_lossy().into_owned(), tracker.hash_file(lf)?);
    }
    // Packages commonly extend or load configs at the workspace root, and
    // which ones cannot be known without running their tools.
    files.extend(tracker.hash_files(&workspace::top_level_files(&ws.root))?);

    let project_dirs = match package {
        // `--filter` selects every project with the name.
        Some(pkg) => {
            let pkg_dirs = ws.package_dirs(pkg);
            if pkg_dirs.is_empty() {
                return Err(format!("pnpm: package '{pkg}' not found in workspace").into());
            }
            pkg_dirs
        }
        None => script_projects(&ws, Path::new(workspace_dir(block))),
    };
    // Projects nested in another are scanned as projects of their own when
    // they are sources, so their build output is skipped at their top.
    let mut exclude: Vec<PathBuf> = outputs.iter().map(|output| block.path(output).into()).collect();
    exclude.extend(ws.projects.iter().map(|project| project.dir.clone()));
    // Each project builds from its local dependencies too.
    for source in ws.local_sources(&project_dirs) {
        let files_in = match source.is_dir() {
            true => {
                let exclude: Vec<PathBuf> = exclude.iter().filter(|dir| **dir != source).cloned().collect();
                workspace::scan_sources(&source, &exclude)
            }
            // A tarball, or a dependency that no longer exists.
            false => vec![source],
        };
        files.extend(tracker.hash_files(&files_in)?);
    }

    for glob in extra_globs {
        files.extend(tracker.hash_glob(&block.glob(glob))?);
    }
    Ok(files)
}

/// The projects a script run in `dir` without `--filter` may read: pnpm runs
/// the script of the project containing `dir`. The workspace root's script
/// can touch any package, as `pnpm -r build` does, so it reads them all.
fn script_projects<'a>(ws: &'a workspace::Workspace, dir: &Path) -> Vec<&'a Path> {
    let dir = dir.canonicalize().unwrap_or_else(|_| dir.to_path_buf());
    let project = ws
        .projects
        .iter()
        .filter(|project| dir.starts_with(&project.dir))
        .max_by_key(|project| project.dir.components().count());
    match project {
        Some(project) if project.dir != ws.root => vec![project.dir.as_path()],
        _ => ws.projects.iter().map(|project| project.dir.as_path()).collect(),
    }
}

/// Run a script defined in `package.json`.
#[derive(Debug, Deserialize, bit_derive::Schema)]
pub struct PnpmRunInputs {
    /// Script name from `package.json` (e.g. "build")
    pub script: String,
    /// Package name from its `package.json`. Omit to run the script of the project in `dir`; at the workspace root, the script may touch any package, so every project's sources are tracked.
    #[serde(default)]
    pub package: Option<String>,
    /// Additional arguments passed to the script after `--`
    #[serde(default)]
    pub args: Vec<String>,
    /// Output file or list of output files/directories produced by the script, relative to `dir`
    #[serde(default, deserialize_with = "crate::providers::string_or_vec")]
    #[schema(typ = crate::schema::optional_string_or_list())]
    pub outputs: Vec<String>,
    /// Extra input file globs, relative to `dir` (added to the files at the workspace root, and the sources of the package and of its `workspace:`, `link:` and `file:` dependencies)
    #[serde(default)]
    pub inputs: Vec<String>,
}

/// Outputs from a `pnpm.run` block.
#[derive(Debug, Serialize, bit_derive::Schema)]
pub struct PnpmRunOutputs {
    /// Single output path, when exactly one was declared
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    /// Multiple output paths, when more than one was declared
    #[serde(skip_serializing_if = "Option::is_none")]
    pub paths: Option<Vec<String>>,
}

impl PnpmRunOutputs {
    pub(super) fn from_paths(outputs: &[String]) -> Self {
        match outputs.len() {
            0 => Self {
                path: None,
                paths: None,
            },
            1 => Self {
                path: Some(outputs[0].clone()),
                paths: None,
            },
            _ => Self {
                path: None,
                paths: Some(outputs.to_vec()),
            },
        }
    }
}

/// Persisted state for a `pnpm.run` block.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PnpmRunState {
    pub script: String,
    pub package: Option<String>,
    pub args: Vec<String>,
    /// The alias reads state written before the field was renamed, so
    /// `bit --clean` still removes outputs those runs declared.
    #[serde(alias = "output")]
    pub outputs: Vec<String>,
}

pub struct PnpmRunResource {
    pub(super) tracker: Arc<FileTracker>,
}

impl Resource for PnpmRunResource {
    type State = PnpmRunState;
    type Inputs = PnpmRunInputs;
    type Outputs = PnpmRunOutputs;

    fn name(&self) -> &str {
        "run"
    }

    fn kind(&self) -> ResourceKind {
        ResourceKind::Build
    }

    fn ambient(&self, inputs: &Block<PnpmRunInputs>) -> Vec<Ambient> {
        super::ambient(workspace_dir(inputs))
    }

    fn resolve(&self, inputs: &Block<PnpmRunInputs>) -> Result<BTreeMap<String, SHA256>, BoxError> {
        let tracker = &*self.tracker;
        resolve_inputs(
            inputs,
            inputs.package.as_deref(),
            &inputs.outputs,
            &inputs.inputs,
            tracker,
        )
    }

    fn plan(&self, inputs: &Block<PnpmRunInputs>, prior_state: Option<&PnpmRunState>) -> Result<PlanResult, BoxError> {
        let description = describe(&inputs.script, inputs.package.as_deref(), &inputs.args);
        let Some(prior) = prior_state else {
            return Ok(PlanResult {
                action: PlanAction::Create,
                description,
                reason: None,
            });
        };
        let action = if prior.script != inputs.script
            || prior.package != inputs.package
            || prior.args != inputs.args
            || prior.outputs != inputs.outputs
        {
            PlanAction::Update
        } else {
            PlanAction::None
        };
        Ok(PlanResult {
            action,
            description,
            reason: None,
        })
    }

    fn apply(
        &self,
        inputs: &Block<PnpmRunInputs>,
        _prior_state: Option<&PnpmRunState>,
        _changes: &Changes,
        writer: &BlockWriter,
    ) -> Result<ApplyResult<PnpmRunState, PnpmRunOutputs>, BoxError> {
        let outputs: Vec<String> = inputs.outputs.iter().map(|output| inputs.path(output)).collect();
        for output in &outputs {
            let p = Path::new(output);
            if output.ends_with('/') {
                fs::create_dir_all(p)?;
            } else if let Some(parent) = p.parent()
                && !parent.as_os_str().is_empty()
            {
                fs::create_dir_all(parent)?;
            }
        }

        let args = pnpm_args(&inputs.script, inputs.package.as_deref(), &inputs.args);
        run_pnpm(&args, Some(workspace_dir(inputs)), writer)?;

        Ok(ApplyResult {
            outputs: PnpmRunOutputs::from_paths(&outputs),
            state: Some(PnpmRunState {
                script: inputs.script.clone(),
                package: inputs.package.clone(),
                args: inputs.args.clone(),
                outputs: inputs.outputs.clone(),
            }),
        })
    }

    fn destroy(&self, prior_state: &Block<PnpmRunState>, writer: &BlockWriter) -> Result<(), BoxError> {
        for output in &prior_state.outputs {
            crate::providers::remove_path(Path::new(&prior_state.path(output)), writer)?;
        }
        Ok(())
    }

    /// A package script is an arbitrary command, like `exec`: the action key
    /// names the script but nothing it invokes. Its declared outputs are also
    /// commonly directories, which the CAS cannot store.
    fn cache_policy(&self, _inputs: &Block<PnpmRunInputs>) -> CachePolicy {
        CachePolicy::Local
    }

    fn output_keys(&self, inputs: &Block<PnpmRunInputs>) -> Vec<String> {
        inputs
            .outputs
            .iter()
            .map(|path| inputs.path(path).trim_end_matches('/').to_owned())
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_resource() -> PnpmRunResource {
        PnpmRunResource {
            tracker: Arc::new(FileTracker::new()),
        }
    }

    #[test]
    fn resource_kind_is_build() {
        assert_eq!(Resource::kind(&test_resource()), ResourceKind::Build);
    }

    #[test]
    fn build_args_without_package() {
        let args = pnpm_args("build", None, &[]);
        assert_eq!(args, vec!["run", "build"]);
    }

    #[test]
    fn build_args_with_package_and_script_args() {
        let args = pnpm_args("build", Some("bff"), &["--mode=production".to_owned()]);
        assert_eq!(args, vec!["--filter", "bff", "run", "build", "--", "--mode=production"]);
    }

    #[test]
    fn describe_matches_args() {
        assert_eq!(describe("build", Some("bff"), &[]), "pnpm --filter bff run build");
        assert_eq!(
            describe("build", None, &["--watch".to_owned()]),
            "pnpm run build -- --watch"
        );
    }

    #[test]
    fn plan_create_when_no_prior_state() {
        let inputs = Block::new(
            None,
            PnpmRunInputs {
                script: "build".into(),
                package: Some("bff".into()),
                args: vec![],
                outputs: vec![],
                inputs: vec![],
            },
        );
        let result = Resource::plan(&test_resource(), &inputs, None).unwrap();
        assert_eq!(result.action, PlanAction::Create);
    }

    #[test]
    fn plan_update_when_script_changed() {
        let inputs = Block::new(
            None,
            PnpmRunInputs {
                script: "test".into(),
                package: Some("bff".into()),
                args: vec![],
                outputs: vec![],
                inputs: vec![],
            },
        );
        let prior = PnpmRunState {
            script: "build".into(),
            package: Some("bff".into()),
            args: vec![],
            outputs: vec![],
        };
        let result = Resource::plan(&test_resource(), &inputs, Some(&prior)).unwrap();
        assert_eq!(result.action, PlanAction::Update);
    }

    #[test]
    fn a_script_without_package_reads_the_project_it_runs_in() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        fs::write(root.join("package.json"), r#"{ "name": "root" }"#).unwrap();
        fs::write(root.join("pnpm-workspace.yaml"), "packages:\n  - 'packages/*'\n").unwrap();
        for name in ["a", "b"] {
            fs::create_dir_all(root.join("packages").join(name).join("src")).unwrap();
            fs::write(
                root.join("packages").join(name).join("package.json"),
                format!(r#"{{ "name": "{name}" }}"#),
            )
            .unwrap();
            fs::write(root.join("packages").join(name).join("src/index.ts"), "").unwrap();
        }
        let resolve = |dir: &Path| {
            let inputs = Block::new(
                Some(dir.to_string_lossy().into_owned()),
                PnpmRunInputs {
                    script: "build".into(),
                    package: None,
                    args: vec![],
                    outputs: vec![],
                    inputs: vec![],
                },
            );
            Resource::resolve(&test_resource(), &inputs).unwrap()
        };
        let tracked = |files: &BTreeMap<String, SHA256>, name: &str| files.keys().any(|p| p.ends_with(name));

        fs::create_dir_all(root.join("packages/a/dist")).unwrap();
        fs::write(root.join("packages/a/dist/index.js"), "").unwrap();
        let at_root = resolve(&root);
        assert!(tracked(&at_root, "packages/a/src/index.ts"), "{at_root:?}");
        assert!(tracked(&at_root, "packages/b/src/index.ts"), "{at_root:?}");
        // Build output at the top of a nested project, not of the root.
        assert!(!tracked(&at_root, "packages/a/dist/index.js"), "{at_root:?}");

        let in_a = resolve(&root.join("packages/a"));
        assert!(tracked(&in_a, "packages/a/src/index.ts"), "{in_a:?}");
        assert!(!tracked(&in_a, "packages/b/src/index.ts"), "{in_a:?}");
    }

    #[test]
    fn a_package_script_reads_the_workspace_root_configs() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        fs::write(root.join("package.json"), r#"{ "name": "root" }"#).unwrap();
        fs::write(root.join("pnpm-workspace.yaml"), "packages:\n  - 'packages/*'\n").unwrap();
        fs::write(root.join("tsconfig.base.json"), "{}").unwrap();
        fs::write(root.join(".env"), "").unwrap();
        fs::create_dir_all(root.join("docs")).unwrap();
        fs::write(root.join("docs/guide.md"), "").unwrap();
        fs::create_dir_all(root.join("packages/a")).unwrap();
        fs::write(root.join("packages/a/package.json"), r#"{ "name": "a" }"#).unwrap();
        let inputs = Block::new(
            Some(root.to_string_lossy().into_owned()),
            PnpmRunInputs {
                script: "build".into(),
                package: Some("a".into()),
                args: vec![],
                outputs: vec![],
                inputs: vec![],
            },
        );

        let files = Resource::resolve(&test_resource(), &inputs).unwrap();
        for config in ["tsconfig.base.json", ".env", "pnpm-workspace.yaml"] {
            assert!(files.contains_key(&*root.join(config).to_string_lossy()), "{files:?}");
        }
        assert!(!files.keys().any(|p| p.ends_with("docs/guide.md")), "{files:?}");
    }

    #[test]
    fn outputs_are_relative_to_dir() {
        let inputs = Block::new(
            Some("web".into()),
            PnpmRunInputs {
                script: "build".into(),
                package: None,
                args: vec![],
                outputs: vec!["dist/".into(), "/abs/out".into()],
                inputs: vec![],
            },
        );
        assert_eq!(
            Resource::output_keys(&test_resource(), &inputs),
            ["web/dist", "/abs/out"]
        );
    }

    #[test]
    fn outputs_single_vs_multiple() {
        let o = PnpmRunOutputs::from_paths(&["bff/dist".to_owned()]);
        assert_eq!(o.path.as_deref(), Some("bff/dist"));
        assert!(o.paths.is_none());
        let o = PnpmRunOutputs::from_paths(&["a".to_owned(), "b".to_owned()]);
        assert!(o.path.is_none());
        assert_eq!(o.paths, Some(vec!["a".into(), "b".into()]));
    }

    #[test]
    fn destroy_removes_declared_outputs() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("bundle.js");
        let output_dir = dir.path().join("dist");
        fs::write(&file, "bundle").unwrap();
        fs::create_dir(&output_dir).unwrap();
        fs::write(output_dir.join("index.html"), "html").unwrap();
        let state = PnpmRunState {
            script: "build".into(),
            package: None,
            args: vec![],
            outputs: vec![
                file.to_string_lossy().into_owned(),
                output_dir.to_string_lossy().into_owned(),
            ],
        };
        let output = crate::output::Output::new(&[]);

        Resource::destroy(&test_resource(), &Block::new(None, state), &output.writer("build")).unwrap();

        assert!(!file.exists());
        assert!(!output_dir.exists());
    }
}
