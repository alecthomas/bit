use std::collections::BTreeMap;
use std::sync::Arc;

use serde::{Deserialize, Serialize};

use crate::file_tracker::FileTracker;
use crate::output::BlockWriter;
use crate::provider::{
    Ambient, ApplyResult, Block, BoxError, CachePolicy, Changes, PlanAction, PlanResult, Resource, ResourceKind,
};
use crate::sha256::SHA256;

use super::run::{describe, pnpm_args, resolve_inputs};
use super::{run_pnpm, workspace_dir};

fn default_script() -> String {
    "test".to_owned()
}

/// Run a test script via pnpm.
#[derive(Debug, Deserialize, bit_derive::Schema)]
pub struct PnpmTestInputs {
    /// Script name from `package.json` (defaults to "test")
    #[serde(default = "default_script")]
    pub script: String,
    /// Package name from its `package.json`. Omit to run the script of the project in `dir`; at the workspace root, the script may touch any package, so every project's sources are tracked.
    #[serde(default)]
    pub package: Option<String>,
    /// Additional arguments passed to the script after `--`
    #[serde(default)]
    pub args: Vec<String>,
    /// Extra input file globs, relative to `dir` (added to the sources of the package and of its `workspace:`, `link:` and `file:` dependencies)
    #[serde(default)]
    pub inputs: Vec<String>,
}

/// Outputs from a `pnpm.test` block.
#[derive(Debug, Serialize, bit_derive::Schema)]
pub struct PnpmTestOutputs {
    /// Whether the test command exited zero
    pub passed: bool,
}

/// Persisted state for a `pnpm.test` block.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PnpmTestState {
    pub script: String,
    pub package: Option<String>,
    pub args: Vec<String>,
}

pub struct PnpmTestResource {
    pub(super) tracker: Arc<FileTracker>,
}

impl Resource for PnpmTestResource {
    type State = PnpmTestState;
    type Inputs = PnpmTestInputs;
    type Outputs = PnpmTestOutputs;

    fn name(&self) -> &str {
        "test"
    }

    fn kind(&self) -> ResourceKind {
        ResourceKind::Test
    }

    fn ambient(&self, inputs: &Block<PnpmTestInputs>) -> Vec<Ambient> {
        super::ambient(workspace_dir(inputs))
    }

    fn resolve(&self, inputs: &Block<PnpmTestInputs>) -> Result<BTreeMap<String, SHA256>, BoxError> {
        let tracker = &*self.tracker;
        resolve_inputs(inputs, inputs.package.as_deref(), &[], &inputs.inputs, tracker)
    }

    fn plan(
        &self,
        inputs: &Block<PnpmTestInputs>,
        prior_state: Option<&PnpmTestState>,
    ) -> Result<PlanResult, BoxError> {
        let description = describe(&inputs.script, inputs.package.as_deref(), &inputs.args);
        let Some(prior) = prior_state else {
            return Ok(PlanResult {
                action: PlanAction::Create,
                description,
                reason: None,
            });
        };
        let action = if prior.script != inputs.script || prior.package != inputs.package || prior.args != inputs.args {
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
        inputs: &Block<PnpmTestInputs>,
        _prior_state: Option<&PnpmTestState>,
        _changes: &Changes,
        writer: &BlockWriter,
    ) -> Result<ApplyResult<PnpmTestState, PnpmTestOutputs>, BoxError> {
        let args = pnpm_args(&inputs.script, inputs.package.as_deref(), &inputs.args);
        let passed = run_pnpm(&args, Some(workspace_dir(inputs)), writer).is_ok();

        Ok(ApplyResult {
            outputs: PnpmTestOutputs { passed },
            state: Some(PnpmTestState {
                script: inputs.script.clone(),
                package: inputs.package.clone(),
                args: inputs.args.clone(),
            }),
        })
    }

    fn destroy(&self, _prior_state: &Block<PnpmTestState>, _writer: &BlockWriter) -> Result<(), BoxError> {
        Ok(())
    }

    /// Local for the same reason as `pnpm.run`: nothing in the action key
    /// describes the tools the test script runs.
    fn cache_policy(&self, _inputs: &Block<PnpmTestInputs>) -> CachePolicy {
        CachePolicy::Local
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_resource() -> PnpmTestResource {
        PnpmTestResource {
            tracker: Arc::new(FileTracker::new()),
        }
    }

    #[test]
    fn resource_kind_is_test() {
        assert_eq!(Resource::kind(&test_resource()), ResourceKind::Test);
    }

    #[test]
    fn plan_create_when_no_prior_state() {
        let inputs = Block::new(
            None,
            PnpmTestInputs {
                script: "test".into(),
                package: Some("bff".into()),
                args: vec![],
                inputs: vec![],
            },
        );
        let result = Resource::plan(&test_resource(), &inputs, None).unwrap();
        assert_eq!(result.action, PlanAction::Create);
        assert_eq!(result.description, "pnpm --filter bff run test");
    }

    #[test]
    fn plan_update_when_args_changed() {
        let inputs = Block::new(
            None,
            PnpmTestInputs {
                script: "test".into(),
                package: None,
                args: vec!["--reporter=verbose".into()],
                inputs: vec![],
            },
        );
        let prior = PnpmTestState {
            script: "test".into(),
            package: None,
            args: vec![],
        };
        let result = Resource::plan(&test_resource(), &inputs, Some(&prior)).unwrap();
        assert_eq!(result.action, PlanAction::Update);
    }
}
