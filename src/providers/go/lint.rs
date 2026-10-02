use std::collections::BTreeMap;
use std::io::BufReader;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};

use serde::{Deserialize, Serialize};

use crate::file_tracker::FileTracker;
use crate::output::BlockWriter;
use crate::provider::{ApplyResult, BoxError, CachePolicy, PlanAction, PlanResult, Resource, ResourceKind};
use crate::sha256::SHA256;

/// Run golangci-lint
#[derive(Debug, Deserialize, bit_derive::Schema)]
pub struct GoLintInputs {
    /// Go package pattern
    #[serde(default = "default_package")]
    pub package: String,
    /// Extra flags passed to golangci-lint run
    #[serde(default)]
    pub flags: Vec<String>,
    /// Working directory for the command
    #[serde(default)]
    pub dir: Option<String>,
}

fn default_package() -> String {
    "./...".to_owned()
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
    pub package: String,
    pub flags: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dir: Option<String>,
}

const GOLANGCI_CONFIG_NAMES: [&str; 4] = [".golangci.yml", ".golangci.yaml", ".golangci.toml", ".golangci.json"];

/// Config files golangci-lint may load when run in `lint_dir` on `package`.
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

pub struct GoLintResource {
    tracker: Arc<Mutex<FileTracker>>,
}

impl GoLintResource {
    pub fn new(tracker: Arc<Mutex<FileTracker>>) -> Self {
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

    fn resolve(&self, inputs: &GoLintInputs) -> Result<BTreeMap<String, SHA256>, BoxError> {
        let mut tracker = self.tracker.lock().expect("tracker lock poisoned");
        let dir = inputs.dir.as_deref().map(Path::new);
        // golangci-lint lints tests by default; tracking them when disabled only costs a rerun.
        let mut files = super::resolve_go_inputs(&inputs.package, dir, true, &mut tracker)?;
        let lint_dir = std::env::current_dir()?.join(dir.unwrap_or(Path::new("")));
        let home = dirs::home_dir();
        for path in golangci_config_files(&lint_dir, &inputs.package, home.as_deref()) {
            let hash = tracker.hash_file(&path)?;
            files.insert(path.to_string_lossy().into_owned(), hash);
        }
        Ok(files)
    }

    fn plan(&self, inputs: &GoLintInputs, prior_state: Option<&GoLintState>) -> Result<PlanResult, BoxError> {
        let description = format!("golangci-lint run {}", inputs.package);

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
        writer: &BlockWriter,
    ) -> Result<ApplyResult<GoLintState, GoLintOutputs>, BoxError> {
        let mut args = vec!["run".to_owned()];
        args.extend(inputs.flags.iter().cloned());
        args.push(inputs.package.clone());

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
            state: Some(GoLintState {
                package: inputs.package.clone(),
                flags: inputs.flags.clone(),
                dir: inputs.dir.clone(),
            }),
        })
    }

    fn destroy(&self, _prior_state: &GoLintState, _writer: &BlockWriter) -> Result<(), BoxError> {
        Ok(())
    }

    /// golangci-lint takes a global lock and exits when another instance holds it.
    fn default_concurrency(&self) -> Option<usize> {
        Some(1)
    }

    fn cache_policy(&self, _inputs: &GoLintInputs) -> CachePolicy {
        CachePolicy::Shared { version: 1 }
    }

    fn toolchain(&self, inputs: &GoLintInputs) -> Result<BTreeMap<String, String>, BoxError> {
        let dir = inputs.dir.as_deref().map(Path::new);
        let mut fingerprint = super::toolchain_fingerprint(&super::GoEnv::default(), dir)?;
        let version = crate::providers::probe_tool("golangci-lint version", || {
            let mut cmd = Command::new("golangci-lint");
            cmd.arg("version");
            cmd
        })?;
        fingerprint.insert("golangci-lint".to_owned(), version);
        Ok(fingerprint)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_resource() -> GoLintResource {
        GoLintResource::new(Arc::new(Mutex::new(FileTracker::default())))
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
            package: "./...".into(),
            flags: vec![],
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
            package: "./...".into(),
            flags: vec![],
            dir: Some(root.to_string_lossy().into_owned()),
        };
        let resolved = Resource::resolve(&test_resource(), &inputs).unwrap();
        assert!(
            resolved.contains_key(&root.join(".golangci.yml").to_string_lossy().into_owned()),
            "{resolved:?}"
        );
    }

    #[test]
    fn resource_kind_is_test() {
        assert_eq!(Resource::kind(&test_resource()), ResourceKind::Test);
    }

    #[test]
    fn plan_create_when_no_prior_state() {
        let inputs = GoLintInputs {
            package: "./...".into(),
            flags: vec![],
            dir: None,
        };
        let result = Resource::plan(&test_resource(), &inputs, None).unwrap();
        assert_eq!(result.action, PlanAction::Create);
    }

    #[test]
    fn plan_none_when_unchanged() {
        let inputs = GoLintInputs {
            package: "./...".into(),
            flags: vec![],
            dir: None,
        };
        let prior = GoLintState {
            package: "./...".into(),
            flags: vec![],
            dir: None,
        };
        let result = Resource::plan(&test_resource(), &inputs, Some(&prior)).unwrap();
        assert_eq!(result.action, PlanAction::None);
    }

    #[test]
    fn plan_update_when_flags_changed() {
        let inputs = GoLintInputs {
            package: "./...".into(),
            flags: vec!["--fast".into()],
            dir: None,
        };
        let prior = GoLintState {
            package: "./...".into(),
            flags: vec![],
            dir: None,
        };
        let result = Resource::plan(&test_resource(), &inputs, Some(&prior)).unwrap();
        assert_eq!(result.action, PlanAction::Update);
    }

    #[test]
    fn default_package_is_all() {
        let inputs: GoLintInputs = serde_json::from_str("{}").unwrap();
        assert_eq!(inputs.package, "./...");
    }
}
