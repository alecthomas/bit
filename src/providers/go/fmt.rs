use crate::process::CommandExt;
use std::collections::BTreeMap;
use std::io::BufReader;
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::Arc;

use serde::{Deserialize, Serialize};

use crate::file_tracker::FileTracker;
use crate::output::BlockWriter;
use crate::provider::{
    Ambient, ApplyResult, Block, BoxError, CachePolicy, Changes, PlanAction, PlanResult, Resource, ResourceKind,
};
use crate::sha256::SHA256;

use super::scanner::Tests;

fn default_package() -> Vec<String> {
    vec!["./...".to_owned()]
}

/// Format Go source files
#[derive(Debug, Deserialize, bit_derive::Schema)]
pub struct GoFmtInputs {
    /// Go package pattern or list of patterns
    #[serde(default = "default_package", deserialize_with = "super::package_patterns")]
    #[schema(typ = crate::schema::optional_string_or_list())]
    pub package: Vec<String>,
}

/// Outputs from a `go.fmt` block.
#[derive(Debug, Serialize, bit_derive::Schema)]
pub struct GoFmtOutputs {}

/// Check Go source formatting
#[derive(Debug, Serialize, bit_derive::Schema)]
pub struct GoFmtCheckOutputs {
    /// Whether all files are formatted
    pub passed: bool,
}

/// Persisted state for `go.fmt` / `go.fmt-l` blocks.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GoFmtState {
    /// A string in state written before `package` took a list.
    #[serde(deserialize_with = "crate::providers::string_or_vec")]
    pub package: Vec<String>,
}

/// Collect `.go` file paths, including `_test.go` files, for the given package patterns.
///
/// `dir` mirrors [`super::scanner::scan`]'s `base_dir`: when set, module-root
/// discovery starts from that directory so a block targeting a separate-module
/// subdirectory scans the right module.
fn go_source_files(patterns: &[String], dir: Option<&std::path::Path>) -> Result<Vec<String>, BoxError> {
    let files = super::scanner::scan(patterns, Tests::Sources, dir)?;
    let mut paths: Vec<String> = files
        .into_iter()
        .filter(|p| p.extension().is_some_and(|e| e == "go"))
        .map(|p| p.to_string_lossy().into_owned())
        .collect();
    paths.sort();
    Ok(paths)
}

/// `gofmt` with `mode` (`-w` or `-l`) over `targets`, for display.
fn gofmt_command(mode: &str, targets: &[String], dir: Option<&str>) -> String {
    let args: Vec<String> = std::iter::once(mode.to_owned())
        .chain(targets.iter().cloned())
        .collect();
    super::display_command("gofmt", &args, dir)
}

/// The block's `.go` files that changed since its last success, or all of
/// them when anything may have changed.
fn changed_go_files(inputs: &Block<GoFmtInputs>, changes: &Changes) -> Result<Vec<String>, BoxError> {
    let files = go_source_files(&inputs.package, inputs.dir.as_deref().map(Path::new))?;
    Ok(match changes {
        Changes::All => files,
        Changes::Paths(changed) => files
            .into_iter()
            .filter(|file| changed.contains(Path::new(file)))
            .collect(),
    })
}

// ── go.fmt (build) ──────────────────────────────────────────────────────

pub struct GoFmtResource {
    tracker: Arc<FileTracker>,
}

impl GoFmtResource {
    pub fn new(tracker: Arc<FileTracker>) -> Self {
        Self { tracker }
    }
}

impl Resource for GoFmtResource {
    type State = GoFmtState;
    type Inputs = GoFmtInputs;
    type Outputs = GoFmtOutputs;

    fn name(&self) -> &str {
        "fmt"
    }

    fn kind(&self) -> ResourceKind {
        ResourceKind::Build
    }

    /// gofmt ships with the Go toolchain, which the go command reports.
    fn ambient(&self, inputs: &Block<GoFmtInputs>) -> Vec<Ambient> {
        vec![super::toolchain(&super::GoEnv::default(), inputs.dir.as_deref())]
    }

    fn resolve(&self, inputs: &Block<GoFmtInputs>) -> Result<BTreeMap<String, SHA256>, BoxError> {
        let tracker = &*self.tracker;
        let dir = inputs.dir.as_deref().map(std::path::Path::new);
        super::resolve_go_inputs(&inputs.package, dir, Tests::Sources, &[], tracker)
    }

    fn plan(&self, inputs: &Block<GoFmtInputs>, prior_state: Option<&GoFmtState>) -> Result<PlanResult, BoxError> {
        let description = gofmt_command("-w", &inputs.package, inputs.dir.as_deref());

        let Some(prior) = prior_state else {
            return Ok(PlanResult {
                action: PlanAction::Create,
                description,
                reason: None,
            });
        };

        let action = if prior.package != inputs.package {
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
        inputs: &Block<GoFmtInputs>,
        _prior_state: Option<&GoFmtState>,
        changes: &Changes,
        writer: &BlockWriter,
    ) -> Result<ApplyResult<GoFmtState, GoFmtOutputs>, BoxError> {
        let files = changed_go_files(inputs, changes)?;
        if files.is_empty() {
            writer.starting("no Go files changed");
            return Ok(ApplyResult {
                outputs: GoFmtOutputs {},
                state: Some(GoFmtState {
                    package: inputs.package.clone(),
                }),
            });
        }

        writer.starting(&gofmt_command(
            "-w",
            &super::display_paths(files.iter().map(Path::new)),
            inputs.dir.as_deref(),
        ));
        let mut cmd = Command::new("gofmt");
        cmd.arg("-w").args(&files).stdout(Stdio::piped()).stderr(Stdio::piped());
        if let Some(dir) = &inputs.dir {
            cmd.current_dir(dir);
        }
        let mut child = cmd
            .logged_spawn()
            .map_err(|e| format!("failed to execute `gofmt`: {e}"))?;

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

        let status = child.wait().map_err(|e| format!("failed to wait for `gofmt`: {e}"))?;
        if !status.success() {
            return Err(format!("`gofmt -w` exited with {status}").into());
        }

        Ok(ApplyResult {
            outputs: GoFmtOutputs {},
            state: Some(GoFmtState {
                package: inputs.package.clone(),
            }),
        })
    }

    fn destroy(&self, _prior_state: &Block<GoFmtState>, _writer: &BlockWriter) -> Result<(), BoxError> {
        Ok(())
    }

    /// The files formatted depend on the changes.
    fn announces_start(&self) -> bool {
        true
    }

    fn cache_policy(&self, _inputs: &Block<GoFmtInputs>) -> CachePolicy {
        CachePolicy::Shared { version: 1 }
    }
}

// ── go.fmt-l (test) ─────────────────────────────────────────────────────

pub struct GoFmtCheckResource {
    tracker: Arc<FileTracker>,
}

impl GoFmtCheckResource {
    pub fn new(tracker: Arc<FileTracker>) -> Self {
        Self { tracker }
    }
}

impl Resource for GoFmtCheckResource {
    type State = GoFmtState;
    type Inputs = GoFmtInputs;
    type Outputs = GoFmtCheckOutputs;

    fn name(&self) -> &str {
        "fmt-l"
    }

    fn kind(&self) -> ResourceKind {
        ResourceKind::Test
    }

    /// gofmt ships with the Go toolchain, which the go command reports.
    fn ambient(&self, inputs: &Block<GoFmtInputs>) -> Vec<Ambient> {
        vec![super::toolchain(&super::GoEnv::default(), inputs.dir.as_deref())]
    }

    fn resolve(&self, inputs: &Block<GoFmtInputs>) -> Result<BTreeMap<String, SHA256>, BoxError> {
        let tracker = &*self.tracker;
        let dir = inputs.dir.as_deref().map(std::path::Path::new);
        super::resolve_go_inputs(&inputs.package, dir, Tests::Sources, &[], tracker)
    }

    fn plan(&self, inputs: &Block<GoFmtInputs>, prior_state: Option<&GoFmtState>) -> Result<PlanResult, BoxError> {
        let description = gofmt_command("-l", &inputs.package, inputs.dir.as_deref());

        let Some(prior) = prior_state else {
            return Ok(PlanResult {
                action: PlanAction::Create,
                description,
                reason: None,
            });
        };

        let action = if prior.package != inputs.package {
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
        inputs: &Block<GoFmtInputs>,
        _prior_state: Option<&GoFmtState>,
        changes: &Changes,
        writer: &BlockWriter,
    ) -> Result<ApplyResult<GoFmtState, GoFmtCheckOutputs>, BoxError> {
        let files = changed_go_files(inputs, changes)?;
        if files.is_empty() {
            writer.starting("no Go files changed");
            return Ok(ApplyResult {
                outputs: GoFmtCheckOutputs { passed: true },
                state: Some(GoFmtState {
                    package: inputs.package.clone(),
                }),
            });
        }

        writer.starting(&gofmt_command(
            "-l",
            &super::display_paths(files.iter().map(Path::new)),
            inputs.dir.as_deref(),
        ));
        let mut cmd = Command::new("gofmt");
        cmd.arg("-l").args(&files);
        if let Some(dir) = &inputs.dir {
            cmd.current_dir(dir);
        }
        let output = cmd
            .logged_output()
            .map_err(|e| format!("failed to execute `gofmt`: {e}"))?;

        let stdout = String::from_utf8_lossy(&output.stdout);
        let unformatted: Vec<&str> = stdout.lines().filter(|l| !l.is_empty()).collect();
        let passed = unformatted.is_empty();

        if !passed {
            for file in &unformatted {
                writer.stderr_line(file);
            }
        }

        Ok(ApplyResult {
            outputs: GoFmtCheckOutputs { passed },
            state: Some(GoFmtState {
                package: inputs.package.clone(),
            }),
        })
    }

    fn destroy(&self, _prior_state: &Block<GoFmtState>, _writer: &BlockWriter) -> Result<(), BoxError> {
        Ok(())
    }

    /// The files checked depend on the changes.
    fn announces_start(&self) -> bool {
        true
    }

    fn cache_policy(&self, _inputs: &Block<GoFmtInputs>) -> CachePolicy {
        CachePolicy::Shared { version: 1 }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_fmt_resource() -> GoFmtResource {
        GoFmtResource::new(Arc::new(FileTracker::default()))
    }

    fn test_fmt_check_resource() -> GoFmtCheckResource {
        GoFmtCheckResource::new(Arc::new(FileTracker::default()))
    }

    const UNFORMATTED_TEST: &str = "package pkg\nfunc helper( ) {}\n";
    const FORMATTED_TEST: &str = "package pkg\n\nfunc helper() {}\n";

    /// Create a module whose only unformatted file is `pkg/lib_test.go`.
    fn module_with_unformatted_test_file() -> (tempfile::TempDir, Block<GoFmtInputs>) {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::write(root.join("go.mod"), "module example.com/test\n").unwrap();
        std::fs::create_dir_all(root.join("pkg")).unwrap();
        std::fs::write(root.join("pkg/lib.go"), "package pkg\n").unwrap();
        std::fs::write(root.join("pkg/lib_test.go"), UNFORMATTED_TEST).unwrap();
        let inputs = Block::new(
            Some(root.to_string_lossy().into_owned()),
            GoFmtInputs {
                package: vec!["./...".into()],
            },
        );
        (dir, inputs)
    }

    #[test]
    fn fmt_resource_kind_is_build() {
        assert_eq!(Resource::kind(&test_fmt_resource()), ResourceKind::Build);
    }

    #[test]
    fn fmt_check_resource_kind_is_test() {
        assert_eq!(Resource::kind(&test_fmt_check_resource()), ResourceKind::Test);
    }

    #[test]
    fn fmt_plan_create_when_no_prior_state() {
        let inputs = Block::new(
            None,
            GoFmtInputs {
                package: vec!["./...".into()],
            },
        );
        let result = Resource::plan(&test_fmt_resource(), &inputs, None).unwrap();
        assert_eq!(result.action, PlanAction::Create);
        assert_eq!(result.description, "gofmt -w ./...");
    }

    #[test]
    fn fmt_check_plan_create_when_no_prior_state() {
        let inputs = Block::new(
            None,
            GoFmtInputs {
                package: vec!["./...".into()],
            },
        );
        let result = Resource::plan(&test_fmt_check_resource(), &inputs, None).unwrap();
        assert_eq!(result.action, PlanAction::Create);
        assert_eq!(result.description, "gofmt -l ./...");
    }

    #[test]
    fn fmt_plan_none_when_unchanged() {
        let inputs = Block::new(
            None,
            GoFmtInputs {
                package: vec!["./...".into()],
            },
        );
        let prior = GoFmtState {
            package: vec!["./...".into()],
        };
        let result = Resource::plan(&test_fmt_resource(), &inputs, Some(&prior)).unwrap();
        assert_eq!(result.action, PlanAction::None);
    }

    #[test]
    fn fmt_plan_update_when_package_changed() {
        let inputs = Block::new(
            None,
            GoFmtInputs {
                package: vec!["./cmd/...".into()],
            },
        );
        let prior = GoFmtState {
            package: vec!["./...".into()],
        };
        let result = Resource::plan(&test_fmt_resource(), &inputs, Some(&prior)).unwrap();
        assert_eq!(result.action, PlanAction::Update);
    }

    #[test]
    fn fmt_apply_formats_test_files() {
        let (dir, inputs) = module_with_unformatted_test_file();
        let output = crate::output::Output::new(&[]);
        Resource::apply(
            &test_fmt_resource(),
            &inputs,
            None,
            &Changes::All,
            &output.writer("fmt"),
        )
        .unwrap();
        let formatted = std::fs::read_to_string(dir.path().join("pkg/lib_test.go")).unwrap();
        assert_eq!(formatted, FORMATTED_TEST);
    }

    #[test]
    fn fmt_check_fails_on_unformatted_test_files() {
        let (_dir, inputs) = module_with_unformatted_test_file();
        let output = crate::output::Output::new(&[]);
        let result = Resource::apply(
            &test_fmt_check_resource(),
            &inputs,
            None,
            &Changes::All,
            &output.writer("fmt-l"),
        )
        .unwrap();
        assert!(!result.outputs.passed);
    }

    #[test]
    fn fmt_check_reads_only_changed_files() {
        let (dir, inputs) = module_with_unformatted_test_file();
        let root = dir.path().canonicalize().unwrap();
        let output = crate::output::Output::new(&[]);
        let check = |name: &str| {
            let changes = Changes::Paths([root.join(name)].into());
            Resource::apply(
                &test_fmt_check_resource(),
                &inputs,
                None,
                &changes,
                &output.writer("fmt-l"),
            )
            .unwrap()
            .outputs
            .passed
        };
        assert!(check("pkg/lib.go"));
        assert!(!check("pkg/lib_test.go"));
    }

    #[test]
    fn resolve_tracks_test_files() {
        let (_dir, inputs) = module_with_unformatted_test_file();
        let fmt_inputs = Resource::resolve(&test_fmt_resource(), &inputs).unwrap();
        let check_inputs = Resource::resolve(&test_fmt_check_resource(), &inputs).unwrap();
        for resolved in [fmt_inputs, check_inputs] {
            assert!(resolved.keys().any(|p| p.ends_with("pkg/lib_test.go")), "{resolved:?}");
        }
    }

    #[test]
    fn default_package_is_all() {
        let inputs: GoFmtInputs = serde_json::from_str("{}").unwrap();
        assert_eq!(inputs.package, ["./..."]);
    }
}
