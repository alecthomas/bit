use std::io::BufReader;
use std::process::{Command, Stdio};

use serde::{Deserialize, Serialize};

use crate::output::BlockWriter;
use crate::provider::{ApplyResult, BoxError, PlanAction, PlanResult, ResolvedFile, Resource, ResourceKind};

use super::GoEnv;

/// Inputs for a `go.test` block.
#[derive(Debug, Deserialize)]
pub struct GoTestInputs {
    /// Go package to test (e.g. "./...").
    pub package: String,
    /// Extra flags passed to `go test`.
    #[serde(default)]
    pub flags: Vec<String>,
    #[serde(flatten)]
    pub env: GoEnv,
}

/// Outputs from a `go.test` block.
#[derive(Debug, Serialize)]
pub struct GoTestOutputs {
    pub passed: bool,
}

/// Persisted state for a `go.test` block.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GoTestState {
    pub package: String,
    pub flags: Vec<String>,
    #[serde(flatten)]
    pub env: GoEnv,
}

pub struct GoTestResource;

impl Resource for GoTestResource {
    type State = GoTestState;
    type Inputs = GoTestInputs;
    type Outputs = GoTestOutputs;

    fn name(&self) -> &str {
        "test"
    }

    fn kind(&self) -> ResourceKind {
        ResourceKind::Test
    }

    fn resolve(&self, inputs: &GoTestInputs) -> Result<Vec<ResolvedFile>, BoxError> {
        super::resolve_go_inputs(&inputs.package, true)
    }

    fn plan(&self, inputs: &GoTestInputs, prior_state: Option<&GoTestState>) -> Result<PlanResult, BoxError> {
        let description = format!("go test {}", inputs.package);

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
        inputs: &GoTestInputs,
        _prior_state: Option<&GoTestState>,
        writer: &BlockWriter,
    ) -> Result<ApplyResult<GoTestState, GoTestOutputs>, BoxError> {
        let mut args = vec!["test".to_owned()];
        args.extend(inputs.flags.iter().cloned());
        args.push(inputs.package.clone());

        let mut cmd = Command::new("go");
        cmd.args(&args).stdout(Stdio::piped()).stderr(Stdio::piped());
        inputs.env.apply_to(&mut cmd);

        let mut child = cmd.spawn().map_err(|e| format!("failed to execute `go test`: {e}"))?;

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

        let status = child.wait().map_err(|e| format!("failed to wait for `go test`: {e}"))?;
        let passed = status.success();

        Ok(ApplyResult {
            outputs: GoTestOutputs { passed },
            state: Some(GoTestState {
                package: inputs.package.clone(),
                flags: inputs.flags.clone(),
                env: inputs.env.clone(),
            }),
        })
    }

    fn destroy(&self, _prior_state: &GoTestState, _writer: &BlockWriter) -> Result<(), BoxError> {
        Ok(())
    }

    fn refresh(&self, prior_state: &GoTestState) -> Result<ApplyResult<GoTestState, GoTestOutputs>, BoxError> {
        Ok(ApplyResult {
            outputs: GoTestOutputs { passed: true },
            state: Some(prior_state.clone()),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resource_kind_is_test() {
        let resource = GoTestResource;
        assert_eq!(Resource::kind(&resource), ResourceKind::Test);
    }

    #[test]
    fn plan_create_when_no_prior_state() {
        let inputs = GoTestInputs {
            package: "./...".into(),
            flags: vec![],
            env: GoEnv::default(),
        };
        let resource = GoTestResource;
        let result = Resource::plan(&resource, &inputs, None).unwrap();
        assert_eq!(result.action, PlanAction::Create);
    }

    #[test]
    fn dyn_resource_deserializes_inputs() {
        use crate::provider::DynResource;
        use crate::value::{Map, Value};

        let mut inputs = Map::new();
        inputs.insert("package".into(), Value::Str("./...".into()));
        inputs.insert(
            "flags".into(),
            Value::List(vec![
                Value::Str("-timeout".into()),
                Value::Str("30s".into()),
                Value::Str("-race".into()),
            ]),
        );

        let resource: Box<dyn DynResource> = Box::new(GoTestResource);
        let result = resource.plan(&inputs, None).unwrap();
        assert_eq!(result.action, PlanAction::Create);
    }

    #[test]
    fn plan_update_when_flags_changed() {
        let inputs = GoTestInputs {
            package: "./...".into(),
            flags: vec!["-v".into()],
            env: GoEnv::default(),
        };
        let prior = GoTestState {
            package: "./...".into(),
            flags: vec![],
            env: GoEnv::default(),
        };
        let resource = GoTestResource;
        let result = Resource::plan(&resource, &inputs, Some(&prior)).unwrap();
        assert_eq!(result.action, PlanAction::Update);
    }
}
