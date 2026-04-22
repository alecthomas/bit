use std::collections::BTreeMap;
use std::io::BufReader;
use std::process::{Command, Stdio};

use serde::{Deserialize, Serialize};

use crate::file_tracker::FileTracker;
use crate::output::BlockWriter;
use crate::provider::{ApplyResult, BoxError, PlanAction, PlanResult, Resource, ResourceKind};
use crate::sha256::SHA256;

fn default_package() -> String {
    "./...".to_owned()
}

/// Format Go source files
#[derive(Debug, Deserialize, bit_derive::Schema)]
pub struct GoFmtInputs {
    /// Go package pattern
    #[serde(default = "default_package")]
    pub package: String,
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
    pub package: String,
}

/// Collect `.go` file paths (excluding test files) for the given package pattern.
fn go_source_files(package: &str) -> Result<Vec<String>, BoxError> {
    let files = super::scanner::scan(package, false)?;
    let mut paths: Vec<String> = files
        .into_iter()
        .filter(|p| p.extension().is_some_and(|e| e == "go"))
        .map(|p| p.to_string_lossy().into_owned())
        .collect();
    paths.sort();
    Ok(paths)
}

// ── go.fmt (build) ──────────────────────────────────────────────────────

pub struct GoFmtResource;

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

    fn resolve(&self, inputs: &GoFmtInputs, tracker: &mut FileTracker) -> Result<BTreeMap<String, SHA256>, BoxError> {
        super::resolve_go_inputs(&inputs.package, false, tracker)
    }

    fn plan(&self, inputs: &GoFmtInputs, prior_state: Option<&GoFmtState>) -> Result<PlanResult, BoxError> {
        let description = format!("gofmt -w {}", inputs.package);

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
        inputs: &GoFmtInputs,
        _prior_state: Option<&GoFmtState>,
        writer: &BlockWriter,
    ) -> Result<ApplyResult<GoFmtState, GoFmtOutputs>, BoxError> {
        let files = go_source_files(&inputs.package)?;
        if files.is_empty() {
            return Ok(ApplyResult {
                outputs: GoFmtOutputs {},
                state: Some(GoFmtState {
                    package: inputs.package.clone(),
                }),
            });
        }

        let mut child = Command::new("gofmt")
            .arg("-w")
            .args(&files)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
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

    fn destroy(&self, _prior_state: &GoFmtState, _writer: &BlockWriter) -> Result<(), BoxError> {
        Ok(())
    }
}

// ── go.fmt-l (test) ─────────────────────────────────────────────────────

pub struct GoFmtCheckResource;

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

    fn resolve(&self, inputs: &GoFmtInputs, tracker: &mut FileTracker) -> Result<BTreeMap<String, SHA256>, BoxError> {
        super::resolve_go_inputs(&inputs.package, false, tracker)
    }

    fn plan(&self, inputs: &GoFmtInputs, prior_state: Option<&GoFmtState>) -> Result<PlanResult, BoxError> {
        let description = format!("gofmt -l {}", inputs.package);

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
        inputs: &GoFmtInputs,
        _prior_state: Option<&GoFmtState>,
        writer: &BlockWriter,
    ) -> Result<ApplyResult<GoFmtState, GoFmtCheckOutputs>, BoxError> {
        let files = go_source_files(&inputs.package)?;
        if files.is_empty() {
            return Ok(ApplyResult {
                outputs: GoFmtCheckOutputs { passed: true },
                state: Some(GoFmtState {
                    package: inputs.package.clone(),
                }),
            });
        }

        let output = Command::new("gofmt")
            .arg("-l")
            .args(&files)
            .output()
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

    fn destroy(&self, _prior_state: &GoFmtState, _writer: &BlockWriter) -> Result<(), BoxError> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fmt_resource_kind_is_build() {
        assert_eq!(Resource::kind(&GoFmtResource), ResourceKind::Build);
    }

    #[test]
    fn fmt_check_resource_kind_is_test() {
        assert_eq!(Resource::kind(&GoFmtCheckResource), ResourceKind::Test);
    }

    #[test]
    fn fmt_plan_create_when_no_prior_state() {
        let inputs = GoFmtInputs {
            package: "./...".into(),
        };
        let result = Resource::plan(&GoFmtResource, &inputs, None).unwrap();
        assert_eq!(result.action, PlanAction::Create);
        assert_eq!(result.description, "gofmt -w ./...");
    }

    #[test]
    fn fmt_check_plan_create_when_no_prior_state() {
        let inputs = GoFmtInputs {
            package: "./...".into(),
        };
        let result = Resource::plan(&GoFmtCheckResource, &inputs, None).unwrap();
        assert_eq!(result.action, PlanAction::Create);
        assert_eq!(result.description, "gofmt -l ./...");
    }

    #[test]
    fn fmt_plan_none_when_unchanged() {
        let inputs = GoFmtInputs {
            package: "./...".into(),
        };
        let prior = GoFmtState {
            package: "./...".into(),
        };
        let result = Resource::plan(&GoFmtResource, &inputs, Some(&prior)).unwrap();
        assert_eq!(result.action, PlanAction::None);
    }

    #[test]
    fn fmt_plan_update_when_package_changed() {
        let inputs = GoFmtInputs {
            package: "./cmd/...".into(),
        };
        let prior = GoFmtState {
            package: "./...".into(),
        };
        let result = Resource::plan(&GoFmtResource, &inputs, Some(&prior)).unwrap();
        assert_eq!(result.action, PlanAction::Update);
    }

    #[test]
    fn default_package_is_all() {
        let inputs: GoFmtInputs = serde_json::from_str("{}").unwrap();
        assert_eq!(inputs.package, "./...");
    }
}
