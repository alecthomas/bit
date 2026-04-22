use std::io::BufReader;
use std::process::{Command, Stdio};

use serde::{Deserialize, Serialize};

use crate::output::BlockWriter;
use crate::provider::{ApplyResult, BoxError, PlanAction, PlanResult, ResolvedFile, Resource, ResourceKind};

use super::GoEnv;

/// Compile Go packages without producing a binary
#[derive(Debug, Deserialize, bit_derive::Schema)]
pub struct GoBuildInputs {
    /// Go package pattern (e.g. "./...")
    pub package: String,
    /// Extra flags passed to go build
    #[serde(default)]
    pub flags: Vec<String>,
    #[serde(flatten)]
    pub env: GoEnv,
}

/// Outputs from a `go.build` block (none meaningful).
#[derive(Debug, Serialize, bit_derive::Schema)]
pub struct GoBuildOutputs {}

/// Persisted state for a `go.build` block.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GoBuildState {
    pub package: String,
    pub flags: Vec<String>,
    #[serde(flatten)]
    pub env: GoEnv,
}

pub struct GoBuildResource;

impl Resource for GoBuildResource {
    type State = GoBuildState;
    type Inputs = GoBuildInputs;
    type Outputs = GoBuildOutputs;

    fn name(&self) -> &str {
        "build"
    }

    fn kind(&self) -> ResourceKind {
        ResourceKind::Build
    }

    fn resolve(&self, inputs: &GoBuildInputs) -> Result<Vec<ResolvedFile>, BoxError> {
        super::resolve_go_inputs(&inputs.package, false)
    }

    fn plan(&self, inputs: &GoBuildInputs, prior_state: Option<&GoBuildState>) -> Result<PlanResult, BoxError> {
        let description = format!("go build {}", inputs.package);

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
        inputs: &GoBuildInputs,
        _prior_state: Option<&GoBuildState>,
        writer: &BlockWriter,
    ) -> Result<ApplyResult<GoBuildState, GoBuildOutputs>, BoxError> {
        let mut args = vec!["build".to_owned()];
        args.extend(inputs.flags.iter().cloned());
        args.push(inputs.package.clone());

        let mut cmd = Command::new("go");
        cmd.args(&args).stdout(Stdio::piped()).stderr(Stdio::piped());
        inputs.env.apply_to(&mut cmd);

        let mut child = cmd.spawn().map_err(|e| format!("failed to execute `go build`: {e}"))?;

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
            .map_err(|e| format!("failed to wait for `go build`: {e}"))?;
        if !status.success() {
            return Err(format!("`go build` exited with {status}").into());
        }

        Ok(ApplyResult {
            outputs: GoBuildOutputs {},
            state: Some(GoBuildState {
                package: inputs.package.clone(),
                flags: inputs.flags.clone(),
                env: inputs.env.clone(),
            }),
        })
    }

    fn destroy(&self, _prior_state: &GoBuildState, _writer: &BlockWriter) -> Result<(), BoxError> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resource_kind_is_build() {
        let resource = GoBuildResource;
        assert_eq!(Resource::kind(&resource), ResourceKind::Build);
    }

    #[test]
    fn plan_create_when_no_prior_state() {
        let inputs = GoBuildInputs {
            package: "./...".into(),
            flags: vec![],
            env: GoEnv::default(),
        };
        let resource = GoBuildResource;
        let result = Resource::plan(&resource, &inputs, None).unwrap();
        assert_eq!(result.action, PlanAction::Create);
    }

    #[test]
    fn plan_none_when_unchanged() {
        let inputs = GoBuildInputs {
            package: "./...".into(),
            flags: vec![],
            env: GoEnv::default(),
        };
        let prior = GoBuildState {
            package: "./...".into(),
            flags: vec![],
            env: GoEnv::default(),
        };
        let resource = GoBuildResource;
        let result = Resource::plan(&resource, &inputs, Some(&prior)).unwrap();
        assert_eq!(result.action, PlanAction::None);
    }
}
