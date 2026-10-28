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
/// target package's source tree (recursively, excluding common build
/// directories and declared outputs) plus the workspace lockfile. When no
/// `package` is given, only the root `package.json` and lockfile are
/// included — root-level scripts rarely need the whole tree hashed.
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

    match package {
        Some(pkg) => {
            let pkg_dirs = ws.package_dirs(pkg);
            if pkg_dirs.is_empty() {
                return Err(format!("pnpm: package '{pkg}' not found in workspace").into());
            }
            let exclude_outputs: Vec<PathBuf> = outputs.iter().map(|output| block.path(output).into()).collect();
            // `--filter` selects every project with the name, and each
            // builds from its local dependencies too.
            for source in ws.local_sources(&pkg_dirs) {
                let files_in = match source.is_dir() {
                    true => workspace::scan_sources(&source, &exclude_outputs),
                    // A tarball, or a dependency that no longer exists.
                    false => vec![source],
                };
                files.extend(tracker.hash_files(&files_in)?);
            }
        }
        None => {
            files.insert(
                ws.root_package_json.to_string_lossy().into_owned(),
                tracker.hash_file(&ws.root_package_json)?,
            );
        }
    }

    for glob in extra_globs {
        files.extend(tracker.hash_glob(&block.glob(glob))?);
    }
    Ok(files)
}

/// Run a script defined in `package.json`.
#[derive(Debug, Deserialize, bit_derive::Schema)]
pub struct PnpmRunInputs {
    /// Script name from `package.json` (e.g. "build")
    pub script: String,
    /// Package name from its `package.json`. Omit to run at the workspace root.
    #[serde(default)]
    pub package: Option<String>,
    /// Additional arguments passed to the script after `--`
    #[serde(default)]
    pub args: Vec<String>,
    /// Output file or list of output files/directories produced by the script, relative to `dir`
    #[serde(default, deserialize_with = "crate::providers::string_or_vec")]
    #[schema(typ = crate::schema::optional_string_or_list())]
    pub outputs: Vec<String>,
    /// Extra input file globs, relative to `dir` (added to the sources of the package and of its `workspace:`, `link:` and `file:` dependencies)
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
