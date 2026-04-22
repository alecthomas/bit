use std::collections::BTreeMap;
use std::io::BufReader;
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};

use serde::{Deserialize, Serialize};

use crate::file_tracker::FileTracker;
use crate::output::BlockWriter;
use crate::provider::{ApplyResult, BoxError, PlanAction, PlanResult, Resource, ResourceKind};
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
        let mut files = super::resolve_go_inputs(&inputs.package, false, &mut tracker)?;
        // Include golangci-lint config if present.
        for name in [".golangci.yml", ".golangci.yaml", ".golangci.toml", ".golangci.json"] {
            let path = Path::new(name);
            if path.exists() {
                let hash = tracker.hash_file(path)?;
                files.insert(name.to_owned(), hash);
            }
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

        let mut child = Command::new("golangci-lint")
            .args(&args)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
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
            }),
        })
    }

    fn destroy(&self, _prior_state: &GoLintState, _writer: &BlockWriter) -> Result<(), BoxError> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_resource() -> GoLintResource {
        GoLintResource::new(Arc::new(Mutex::new(FileTracker::default())))
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
        };
        let result = Resource::plan(&test_resource(), &inputs, None).unwrap();
        assert_eq!(result.action, PlanAction::Create);
    }

    #[test]
    fn plan_none_when_unchanged() {
        let inputs = GoLintInputs {
            package: "./...".into(),
            flags: vec![],
        };
        let prior = GoLintState {
            package: "./...".into(),
            flags: vec![],
        };
        let result = Resource::plan(&test_resource(), &inputs, Some(&prior)).unwrap();
        assert_eq!(result.action, PlanAction::None);
    }

    #[test]
    fn plan_update_when_flags_changed() {
        let inputs = GoLintInputs {
            package: "./...".into(),
            flags: vec!["--fast".into()],
        };
        let prior = GoLintState {
            package: "./...".into(),
            flags: vec![],
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
