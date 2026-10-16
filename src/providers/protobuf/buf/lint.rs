use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use serde::{Deserialize, Serialize};

use crate::file_tracker::FileTracker;
use crate::output::BlockWriter;
use crate::provider::{Ambient, ApplyResult, BoxError, CachePolicy, PlanResult, Resource, ResourceKind};
use crate::sha256::SHA256;

use super::BufTarget;

/// Lint Protobuf files with buf lint
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, bit_derive::Schema)]
pub struct BufLintInputs {
    #[serde(flatten)]
    pub target: BufTarget,
    /// Extra flags passed to buf lint
    #[serde(default)]
    pub flags: Vec<String>,
}

/// Outputs from a `buf.lint` block.
#[derive(Debug, Serialize, bit_derive::Schema)]
pub struct BufLintOutputs {
    /// Whether linting passed
    pub passed: bool,
}

pub struct BufLintResource {
    tracker: Arc<Mutex<FileTracker>>,
}

impl BufLintResource {
    pub fn new(tracker: Arc<Mutex<FileTracker>>) -> Self {
        Self { tracker }
    }
}

fn lint_args(inputs: &BufLintInputs) -> Vec<String> {
    let mut args = vec!["lint".to_owned()];
    args.extend(inputs.flags.iter().cloned());
    args
}

impl Resource for BufLintResource {
    type State = BufLintInputs;
    type Inputs = BufLintInputs;
    type Outputs = BufLintOutputs;

    fn name(&self) -> &str {
        "lint"
    }

    fn kind(&self) -> ResourceKind {
        ResourceKind::Test
    }

    fn resolve(&self, inputs: &BufLintInputs) -> Result<BTreeMap<String, SHA256>, BoxError> {
        let mut tracker = self.tracker.lock().expect("tracker lock poisoned");
        inputs.target.resolve(&[], &mut tracker)
    }

    fn ambient(&self, inputs: &BufLintInputs) -> Vec<Ambient> {
        inputs.target.check_ambient()
    }

    fn plan(&self, inputs: &BufLintInputs, prior_state: Option<&BufLintInputs>) -> Result<PlanResult, BoxError> {
        Ok(super::super::plan(
            inputs.target.describe(&lint_args(inputs)),
            prior_state,
            inputs,
        ))
    }

    fn apply(
        &self,
        inputs: &BufLintInputs,
        _prior_state: Option<&BufLintInputs>,
        writer: &BlockWriter,
    ) -> Result<ApplyResult<BufLintInputs, BufLintOutputs>, BoxError> {
        let status = inputs.target.run(&lint_args(inputs), writer)?;
        Ok(ApplyResult {
            outputs: BufLintOutputs {
                passed: status.success(),
            },
            state: Some(inputs.clone()),
        })
    }

    fn destroy(&self, _prior_state: &BufLintInputs, _writer: &BlockWriter) -> Result<(), BoxError> {
        Ok(())
    }

    /// The buf and plugin executables are tools, so the action key already
    /// describes the toolchain.
    fn cache_policy(&self, _inputs: &BufLintInputs) -> CachePolicy {
        CachePolicy::Shared { version: 1 }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::PlanAction;

    fn test_resource() -> BufLintResource {
        BufLintResource::new(Arc::new(Mutex::new(FileTracker::default())))
    }

    #[test]
    fn resource_kind_is_test() {
        assert_eq!(Resource::kind(&test_resource()), ResourceKind::Test);
    }

    #[test]
    fn plan_describes_command_and_updates_when_flags_change() {
        let inputs: BufLintInputs =
            serde_json::from_str(r#"{"input": "proto", "flags": ["--error-format=json"]}"#).unwrap();
        let result = Resource::plan(&test_resource(), &inputs, None).unwrap();
        assert_eq!(result.action, PlanAction::Create);
        assert_eq!(result.description, "buf lint --error-format=json proto");

        let prior: BufLintInputs = serde_json::from_str(r#"{"input": "proto"}"#).unwrap();
        let result = Resource::plan(&test_resource(), &inputs, Some(&prior)).unwrap();
        assert_eq!(result.action, PlanAction::Update);
    }
}
