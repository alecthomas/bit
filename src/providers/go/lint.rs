use std::collections::BTreeMap;
use std::io::BufReader;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;

use serde::{Deserialize, Serialize};

use crate::file_tracker::FileTracker;
use crate::output::BlockWriter;
use crate::provider::{
    Ambient, ApplyResult, BoxError, CachePolicy, Changes, PlanAction, PlanResult, Resource, ResourceKind,
};
use crate::sha256::SHA256;

use super::scanner::Tests;

/// Run golangci-lint
#[derive(Debug, Deserialize, bit_derive::Schema)]
pub struct GoLintInputs {
    /// Go package pattern or list of patterns
    #[serde(default = "default_package", deserialize_with = "super::package_patterns")]
    #[schema(typ = crate::schema::optional_string_or_list())]
    pub package: Vec<String>,
    /// Extra flags passed to golangci-lint run
    #[serde(default)]
    pub flags: Vec<String>,
    /// Input file glob patterns (in addition to Go sources)
    #[serde(default)]
    pub inputs: Vec<String>,
    /// Working directory for the command
    #[serde(default)]
    pub dir: Option<String>,
}

fn default_package() -> Vec<String> {
    vec!["./...".to_owned()]
}

/// Outputs from a `go.lint` block.
#[derive(Debug, Serialize, bit_derive::Schema)]
pub struct GoLintOutputs {
    /// Whether linting passed
    pub passed: bool,
}

/// Persisted state for a `go.lint` block.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GoLintState {
    /// A string in state written before `package` took a list.
    #[serde(deserialize_with = "crate::providers::string_or_vec")]
    pub package: Vec<String>,
    pub flags: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dir: Option<String>,
}

const GOLANGCI_CONFIG_NAMES: [&str; 4] = [".golangci.yml", ".golangci.yaml", ".golangci.toml", ".golangci.json"];

/// Config files golangci-lint may load when run in `lint_dir` on `package`,
/// the first package argument.
///
/// Mirrors golangci-lint: `lint_dir`, the package dir and its ancestors, then
/// `home`. All candidates in the first matching dir are returned.
fn golangci_config_files(lint_dir: &Path, package: &str, home: Option<&Path>) -> Vec<PathBuf> {
    // Collecting components drops `.` segments, as Go's `filepath.Abs` does.
    let package_path: PathBuf = lint_dir.join(package).components().collect();
    let package_dir = if package_path.is_dir() {
        package_path.as_path()
    } else {
        package_path.parent().unwrap_or(lint_dir)
    };
    let search_dirs = std::iter::once(lint_dir).chain(package_dir.ancestors()).chain(home);
    for dir in search_dirs {
        let files: Vec<PathBuf> = GOLANGCI_CONFIG_NAMES
            .iter()
            .map(|name| dir.join(name))
            .filter(|path| path.is_file())
            .collect();
        if !files.is_empty() {
            return files;
        }
    }
    Vec::new()
}

/// The directories of the packages a narrowed run lints, out of the `total`
/// the pattern names.
struct Affected {
    dirs: Vec<PathBuf>,
    total: usize,
}

/// The packages whose own or imported files include a changed path, or
/// `None` to lint the whole pattern.
fn affected_packages(inputs: &GoLintInputs, changes: &Changes) -> Result<Option<Affected>, BoxError> {
    let env = super::GoEnv::default();
    let Some(affected) = super::affected_packages(&inputs.package, inputs.dir.as_deref(), &[], &env, changes)? else {
        return Ok(None);
    };
    // Build tags from flags or the golangci-lint config may include a package
    // the listing excludes, so a change to one lints the whole pattern.
    if affected.excluded {
        return Ok(None);
    }
    // golangci-lint resolves a bare import path as a directory, so packages
    // are named by their absolute directories.
    Ok(Some(Affected {
        dirs: affected.packages.into_iter().map(|p| p.dir).collect(),
        total: affected.total,
    }))
}

/// The arguments to `golangci-lint` that lint `packages`.
fn lint_args(inputs: &GoLintInputs, packages: &[String]) -> Vec<String> {
    let mut args = vec!["run".to_owned()];
    args.extend(inputs.flags.iter().cloned());
    args.extend(packages.iter().cloned());
    args
}

pub struct GoLintResource {
    tracker: Arc<FileTracker>,
}

impl GoLintResource {
    pub fn new(tracker: Arc<FileTracker>) -> Self {
        Self { tracker }
    }
}

impl Resource for GoLintResource {
    type State = GoLintState;
    type Inputs = GoLintInputs;
    type Outputs = GoLintOutputs;

    fn name(&self) -> &str {
        "lint"
    }

    fn kind(&self) -> ResourceKind {
        ResourceKind::Test
    }

    fn ambient(&self, inputs: &GoLintInputs) -> Vec<Ambient> {
        let dir = inputs.dir.as_deref();
        vec![
            crate::providers::version("golangci-lint", &["version"], dir),
            super::toolchain(&super::GoEnv::default(), dir),
        ]
    }

    fn resolve(&self, inputs: &GoLintInputs) -> Result<BTreeMap<String, SHA256>, BoxError> {
        let tracker = &*self.tracker;
        let dir = inputs.dir.as_deref().map(Path::new);
        // golangci-lint lints tests by default; tracking them when disabled only costs a rerun.
        let mut files = super::resolve_go_inputs(&inputs.package, dir, Tests::Sources, &inputs.inputs, tracker)?;
        let lint_dir = std::env::current_dir()?.join(dir.unwrap_or(Path::new("")));
        let home = dirs::home_dir();
        // golangci-lint searches for its config from the first package only.
        for path in golangci_config_files(&lint_dir, &inputs.package[0], home.as_deref()) {
            let hash = tracker.hash_file(&path)?;
            files.insert(path.to_string_lossy().into_owned(), hash);
        }
        Ok(files)
    }

    fn plan(&self, inputs: &GoLintInputs, prior_state: Option<&GoLintState>) -> Result<PlanResult, BoxError> {
        let description = crate::providers::display_command("golangci-lint", &lint_args(inputs, &inputs.package));

        let Some(prior) = prior_state else {
            return Ok(PlanResult {
                action: PlanAction::Create,
                description,
                reason: None,
            });
        };

        let action = if prior.package != inputs.package || prior.flags != inputs.flags {
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
        inputs: &GoLintInputs,
        _prior_state: Option<&GoLintState>,
        changes: &Changes,
        writer: &BlockWriter,
    ) -> Result<ApplyResult<GoLintState, GoLintOutputs>, BoxError> {
        let state = GoLintState {
            package: inputs.package.clone(),
            flags: inputs.flags.clone(),
            dir: inputs.dir.clone(),
        };
        let affected = affected_packages(inputs, changes)?;
        if let Some(Affected { dirs, total }) = &affected
            && dirs.is_empty()
        {
            writer.starting(&format!("0 of {total} packages affected"));
            return Ok(ApplyResult {
                outputs: GoLintOutputs { passed: true },
                state: Some(state),
            });
        }
        let (packages, shown) = match &affected {
            Some(Affected { dirs, .. }) => (
                dirs.iter().map(|dir| dir.to_string_lossy().into_owned()).collect(),
                super::display_paths(dirs.iter().map(PathBuf::as_path)),
            ),
            None => (inputs.package.clone(), inputs.package.clone()),
        };
        let args = lint_args(inputs, &packages);
        writer.starting(&crate::providers::display_command(
            "golangci-lint",
            &lint_args(inputs, &shown),
        ));
        if let Some(Affected { dirs, total }) = &affected {
            writer.line(&format!("{} of {total} packages affected", dirs.len()));
        }

        let mut cmd = Command::new("golangci-lint");
        cmd.args(&args).stdout(Stdio::piped()).stderr(Stdio::piped());
        if let Some(dir) = &inputs.dir {
            cmd.current_dir(dir);
        }
        let mut child = cmd
            .spawn()
            .map_err(|e| format!("failed to execute `golangci-lint`: {e}"))?;

        let stdout = child.stdout.take();
        let stderr = child.stderr.take();
        std::thread::scope(|s| {
            if let Some(out) = stdout {
                s.spawn(|| writer.pipe_stdout(BufReader::new(out)));
            }
            if let Some(err) = stderr {
                s.spawn(|| writer.pipe_stderr(BufReader::new(err)));
            }
        });

        let status = child
            .wait()
            .map_err(|e| format!("failed to wait for `golangci-lint`: {e}"))?;
        let passed = status.success();

        Ok(ApplyResult {
            outputs: GoLintOutputs { passed },
            state: Some(state),
        })
    }

    fn destroy(&self, _prior_state: &GoLintState, _writer: &BlockWriter) -> Result<(), BoxError> {
        Ok(())
    }

    /// The packages linted depend on the changes.
    fn announces_start(&self) -> bool {
        true
    }

    /// golangci-lint takes a global lock and exits when another instance holds it.
    fn default_concurrency(&self) -> Option<usize> {
        Some(1)
    }

    fn cache_policy(&self, _inputs: &GoLintInputs) -> CachePolicy {
        CachePolicy::Shared { version: 1 }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_resource() -> GoLintResource {
        GoLintResource::new(Arc::new(FileTracker::default()))
    }

    #[test]
    fn config_found_in_block_dir() {
        let dir = tempfile::tempdir().unwrap();
        let config = dir.path().join(".golangci.yml");
        std::fs::write(&config, "version: \"2\"\n").unwrap();
        assert_eq!(golangci_config_files(dir.path(), "./...", None), vec![config]);
    }

    #[test]
    fn config_found_in_ancestor_of_block_dir() {
        let dir = tempfile::tempdir().unwrap();
        let config = dir.path().join(".golangci.yaml");
        std::fs::write(&config, "version: \"2\"\n").unwrap();
        let module = dir.path().join("module");
        std::fs::create_dir_all(module.join("pkg")).unwrap();
        assert_eq!(golangci_config_files(&module, "./pkg/...", None), vec![config]);
    }

    #[test]
    fn config_in_block_dir_takes_precedence_over_ancestor() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(".golangci.yml"), "version: \"2\"\n").unwrap();
        let module = dir.path().join("module");
        std::fs::create_dir_all(&module).unwrap();
        let config = module.join(".golangci.toml");
        std::fs::write(&config, "version = \"2\"\n").unwrap();
        assert_eq!(golangci_config_files(&module, "./...", None), vec![config]);
    }

    #[test]
    fn config_falls_back_to_home() {
        let dir = tempfile::tempdir().unwrap();
        let module = dir.path().join("module");
        let home = dir.path().join("home");
        std::fs::create_dir_all(&module).unwrap();
        std::fs::create_dir_all(&home).unwrap();
        let config = home.join(".golangci.json");
        std::fs::write(&config, "{}").unwrap();
        assert_eq!(golangci_config_files(&module, "./...", Some(&home)), vec![config]);
    }

    #[test]
    fn resolve_tracks_test_files() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::write(root.join("go.mod"), "module example.com/test\n").unwrap();
        std::fs::write(root.join("lib.go"), "package lib\n").unwrap();
        std::fs::write(root.join("lib_test.go"), "package lib\n").unwrap();
        let inputs = GoLintInputs {
            package: vec!["./...".into()],
            flags: vec![],
            inputs: vec![],
            dir: Some(root.to_string_lossy().into_owned()),
        };
        let resolved = Resource::resolve(&test_resource(), &inputs).unwrap();
        assert!(resolved.keys().any(|p| p.ends_with("lib_test.go")), "{resolved:?}");
    }

    #[test]
    fn resolve_tracks_config_in_block_dir() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::write(root.join("go.mod"), "module example.com/test\n").unwrap();
        std::fs::write(root.join("main.go"), "package main\n").unwrap();
        std::fs::write(root.join(".golangci.yml"), "version: \"2\"\n").unwrap();
        let inputs = GoLintInputs {
            package: vec!["./...".into()],
            flags: vec![],
            inputs: vec![],
            dir: Some(root.to_string_lossy().into_owned()),
        };
        let resolved = Resource::resolve(&test_resource(), &inputs).unwrap();
        assert!(
            resolved.contains_key(&root.join(".golangci.yml").to_string_lossy().into_owned()),
            "{resolved:?}"
        );
    }

    #[test]
    fn affected_packages_follow_changed_paths() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        std::fs::write(root.join("go.mod"), "module example.com/m\n").unwrap();
        std::fs::write(root.join(".golangci.yml"), "version: \"2\"\n").unwrap();
        std::fs::write(root.join("main.go"), "package main\nimport \"example.com/m/lib\"\n").unwrap();
        std::fs::create_dir_all(root.join("lib")).unwrap();
        std::fs::write(root.join("lib/lib.go"), "package lib\n").unwrap();
        std::fs::create_dir_all(root.join("util")).unwrap();
        std::fs::write(root.join("util/util.go"), "package util\n").unwrap();
        // The default listing excludes this package, but build tags may not.
        std::fs::create_dir_all(root.join("tagged")).unwrap();
        std::fs::write(root.join("tagged/tagged.go"), "//go:build extra\n\npackage tagged\n").unwrap();
        let inputs = GoLintInputs {
            package: vec!["./...".into()],
            flags: vec![],
            inputs: vec![],
            dir: Some(root.to_string_lossy().into_owned()),
        };
        let affected = |names: &[&str]| {
            let changes = Changes::Paths(names.iter().map(|name| root.join(name)).collect());
            affected_packages(&inputs, &changes).unwrap().map(|affected| {
                assert_eq!(affected.total, 3);
                let mut dirs = affected.dirs;
                dirs.sort();
                dirs
            })
        };

        assert_eq!(affected(&["lib/lib.go"]).unwrap(), [root.clone(), root.join("lib")]);
        assert_eq!(affected(&["util/util.go"]).unwrap(), [root.join("util")]);
        assert_eq!(affected(&["tagged/tagged.go"]), None);
        assert_eq!(affected(&[".golangci.yml"]), None);
        assert!(affected_packages(&inputs, &Changes::All).unwrap().is_none());
    }

    #[test]
    fn resource_kind_is_test() {
        assert_eq!(Resource::kind(&test_resource()), ResourceKind::Test);
    }

    #[test]
    fn plan_create_when_no_prior_state() {
        let inputs = GoLintInputs {
            package: vec!["./...".into()],
            flags: vec![],
            inputs: vec![],
            dir: None,
        };
        let result = Resource::plan(&test_resource(), &inputs, None).unwrap();
        assert_eq!(result.action, PlanAction::Create);
    }

    #[test]
    fn plan_none_when_unchanged() {
        let inputs = GoLintInputs {
            package: vec!["./...".into()],
            flags: vec![],
            inputs: vec![],
            dir: None,
        };
        let prior = GoLintState {
            package: vec!["./...".into()],
            flags: vec![],
            dir: None,
        };
        let result = Resource::plan(&test_resource(), &inputs, Some(&prior)).unwrap();
        assert_eq!(result.action, PlanAction::None);
    }

    #[test]
    fn plan_update_when_flags_changed() {
        let inputs = GoLintInputs {
            package: vec!["./...".into()],
            flags: vec!["--fast".into()],
            inputs: vec![],
            dir: None,
        };
        let prior = GoLintState {
            package: vec!["./...".into()],
            flags: vec![],
            dir: None,
        };
        let result = Resource::plan(&test_resource(), &inputs, Some(&prior)).unwrap();
        assert_eq!(result.action, PlanAction::Update);
    }

    #[test]
    fn default_package_is_all() {
        let inputs: GoLintInputs = serde_json::from_str("{}").unwrap();
        assert_eq!(inputs.package, ["./..."]);
    }
}
