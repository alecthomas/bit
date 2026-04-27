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

use super::GoEnv;

/// Run go generate
#[derive(Debug, Deserialize, bit_derive::Schema)]
pub struct GoGenerateInputs {
    /// Go package pattern (e.g. "./...")
    pub package: String,
    /// Extra flags passed to go generate
    #[serde(default)]
    pub flags: Vec<String>,
    /// Input file glob patterns (in addition to Go sources)
    #[serde(default)]
    pub inputs: Vec<String>,
    /// Output file paths produced by generate commands
    #[serde(default)]
    pub outputs: Vec<String>,
    /// Working directory for the command
    #[serde(default)]
    pub dir: Option<String>,
    #[serde(flatten)]
    pub env: GoEnv,
}

/// Outputs from a `go.generate` block (none meaningful).
#[derive(Debug, Serialize, bit_derive::Schema)]
pub struct GoGenerateOutputs {}

/// Persisted state for a `go.generate` block.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GoGenerateState {
    pub package: String,
    pub flags: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dir: Option<String>,
    #[serde(flatten)]
    pub env: GoEnv,
}

pub struct GoGenerateResource {
    tracker: Arc<Mutex<FileTracker>>,
}

impl GoGenerateResource {
    pub fn new(tracker: Arc<Mutex<FileTracker>>) -> Self {
        Self { tracker }
    }
}

impl Resource for GoGenerateResource {
    type State = GoGenerateState;
    type Inputs = GoGenerateInputs;
    type Outputs = GoGenerateOutputs;

    fn name(&self) -> &str {
        "generate"
    }

    fn kind(&self) -> ResourceKind {
        ResourceKind::Build
    }

    fn resolve(&self, inputs: &GoGenerateInputs) -> Result<BTreeMap<String, SHA256>, BoxError> {
        let mut tracker = self.tracker.lock().expect("tracker lock poisoned");
        let mut files = super::resolve_go_inputs(&inputs.package, false, &mut tracker)?;
        for pattern in &inputs.inputs {
            files.extend(tracker.hash_glob(pattern)?);
        }
        for output in &inputs.outputs {
            let path = Path::new(output);
            if path.is_file() {
                files.insert(output.clone(), tracker.hash_file(path)?);
            }
        }
        Ok(files)
    }

    fn plan(&self, inputs: &GoGenerateInputs, prior_state: Option<&GoGenerateState>) -> Result<PlanResult, BoxError> {
        let description = format!("go generate {}", inputs.package);

        let Some(prior) = prior_state else {
            return Ok(PlanResult {
                action: PlanAction::Create,
                description,
                reason: None,
            });
        };

        let action = if prior.package != inputs.package || prior.flags != inputs.flags || prior.env != inputs.env {
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
        inputs: &GoGenerateInputs,
        _prior_state: Option<&GoGenerateState>,
        writer: &BlockWriter,
    ) -> Result<ApplyResult<GoGenerateState, GoGenerateOutputs>, BoxError> {
        let mut args = vec!["generate".to_owned()];
        args.extend(inputs.flags.iter().cloned());
        args.push(inputs.package.clone());

        let mut cmd = Command::new("go");
        cmd.args(&args).stdout(Stdio::piped()).stderr(Stdio::piped());
        if let Some(dir) = &inputs.dir {
            cmd.current_dir(dir);
        }
        inputs.env.apply_to(&mut cmd);

        let mut child = cmd
            .spawn()
            .map_err(|e| format!("failed to execute `go generate`: {e}"))?;

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
            .map_err(|e| format!("failed to wait for `go generate`: {e}"))?;
        if !status.success() {
            return Err(format!("`go generate` exited with {status}").into());
        }

        Ok(ApplyResult {
            outputs: GoGenerateOutputs {},
            state: Some(GoGenerateState {
                package: inputs.package.clone(),
                flags: inputs.flags.clone(),
                dir: inputs.dir.clone(),
                env: inputs.env.clone(),
            }),
        })
    }

    fn destroy(&self, _prior_state: &GoGenerateState, _writer: &BlockWriter) -> Result<(), BoxError> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_resource() -> GoGenerateResource {
        GoGenerateResource::new(Arc::new(Mutex::new(FileTracker::default())))
    }

    #[test]
    fn resource_kind_is_build() {
        assert_eq!(Resource::kind(&test_resource()), ResourceKind::Build);
    }

    #[test]
    fn plan_create_when_no_prior_state() {
        let inputs = GoGenerateInputs {
            package: "./...".into(),
            flags: vec![],
            inputs: vec![],
            outputs: vec![],
            dir: None,
            env: GoEnv::default(),
        };
        let result = Resource::plan(&test_resource(), &inputs, None).unwrap();
        assert_eq!(result.action, PlanAction::Create);
    }

    #[test]
    fn plan_none_when_unchanged() {
        let inputs = GoGenerateInputs {
            package: "./...".into(),
            flags: vec![],
            inputs: vec![],
            outputs: vec![],
            dir: None,
            env: GoEnv::default(),
        };
        let prior = GoGenerateState {
            package: "./...".into(),
            flags: vec![],
            dir: None,
            env: GoEnv::default(),
        };
        let result = Resource::plan(&test_resource(), &inputs, Some(&prior)).unwrap();
        assert_eq!(result.action, PlanAction::None);
    }

    #[test]
    fn plan_update_when_flags_changed() {
        let inputs = GoGenerateInputs {
            package: "./...".into(),
            flags: vec!["-v".into()],
            inputs: vec![],
            outputs: vec![],
            dir: None,
            env: GoEnv::default(),
        };
        let prior = GoGenerateState {
            package: "./...".into(),
            flags: vec![],
            dir: None,
            env: GoEnv::default(),
        };
        let result = Resource::plan(&test_resource(), &inputs, Some(&prior)).unwrap();
        assert_eq!(result.action, PlanAction::Update);
    }
}
