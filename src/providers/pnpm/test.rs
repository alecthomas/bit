use std::collections::BTreeMap;
use std::sync::Arc;

use serde::{Deserialize, Serialize};

use crate::file_tracker::FileTracker;
use crate::output::BlockWriter;
use crate::provider::{
    Ambient, ApplyResult, BoxError, CachePolicy, Changes, PlanAction, PlanResult, Resource, ResourceKind,
};
use crate::sha256::SHA256;

use super::run::{describe, pnpm_args, resolve_inputs};
use super::run_pnpm;

fn default_dir() -> String {
    ".".to_owned()
}

fn default_script() -> String {
    "test".to_owned()
}

/// Run a test script via pnpm.
#[derive(Debug, Deserialize, bit_derive::Schema)]
pub struct PnpmTestInputs {
    /// Script name from `package.json` (defaults to "test")
    #[serde(default = "default_script")]
    pub script: String,
    /// Package name from its `package.json`. Omit to run at the workspace root.
    #[serde(default)]
    pub package: Option<String>,
    /// Additional arguments passed to the script after `--`
    #[serde(default)]
    pub args: Vec<String>,
    /// Extra input file globs (added to auto-detected sources)
    #[serde(default)]
    pub inputs: Vec<String>,
    /// Workspace root directory (defaults to the current directory)
    #[serde(default = "default_dir")]
    pub dir: String,
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
    pub dir: String,
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

    fn ambient(&self, inputs: &PnpmTestInputs) -> Vec<Ambient> {
        super::ambient(&inputs.dir)
    }

    fn resolve(&self, inputs: &PnpmTestInputs) -> Result<BTreeMap<String, SHA256>, BoxError> {
        let tracker = &*self.tracker;
        resolve_inputs(&inputs.dir, inputs.package.as_deref(), &[], &inputs.inputs, tracker)
    }

    fn plan(&self, inputs: &PnpmTestInputs, prior_state: Option<&PnpmTestState>) -> Result<PlanResult, BoxError> {
        let description = describe(&inputs.script, inputs.package.as_deref(), &inputs.args);
        let Some(prior) = prior_state else {
            return Ok(PlanResult {
                action: PlanAction::Create,
                description,
                reason: None,
            });
        };
        let action = if prior.script != inputs.script
            || prior.package != inputs.package
            || prior.args != inputs.args
            || prior.dir != inputs.dir
        {
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
        inputs: &PnpmTestInputs,
        _prior_state: Option<&PnpmTestState>,
        _changes: &Changes,
        writer: &BlockWriter,
    ) -> Result<ApplyResult<PnpmTestState, PnpmTestOutputs>, BoxError> {
        let args = pnpm_args(&inputs.script, inputs.package.as_deref(), &inputs.args);
        let passed = run_pnpm(&args, Some(&inputs.dir), writer).is_ok();

        Ok(ApplyResult {
            outputs: PnpmTestOutputs { passed },
            state: Some(PnpmTestState {
                script: inputs.script.clone(),
                package: inputs.package.clone(),
                args: inputs.args.clone(),
                dir: inputs.dir.clone(),
            }),
        })
    }

    fn destroy(&self, _prior_state: &PnpmTestState, _writer: &BlockWriter) -> Result<(), BoxError> {
        Ok(())
    }

    /// Local for the same reason as `pnpm.run`: nothing in the action key
    /// describes the tools the test script runs.
    fn cache_policy(&self, _inputs: &PnpmTestInputs) -> CachePolicy {
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
        let inputs = PnpmTestInputs {
            script: "test".into(),
            package: Some("bff".into()),
            args: vec![],
            inputs: vec![],
            dir: ".".into(),
        };
        let result = Resource::plan(&test_resource(), &inputs, None).unwrap();
        assert_eq!(result.action, PlanAction::Create);
        assert_eq!(result.description, "pnpm --filter bff run test");
    }

    #[test]
    fn plan_update_when_args_changed() {
        let inputs = PnpmTestInputs {
            script: "test".into(),
            package: None,
            args: vec!["--reporter=verbose".into()],
            inputs: vec![],
            dir: ".".into(),
        };
        let prior = PnpmTestState {
            script: "test".into(),
            package: None,
            args: vec![],
            dir: ".".into(),
        };
        let result = Resource::plan(&test_resource(), &inputs, Some(&prior)).unwrap();
        assert_eq!(result.action, PlanAction::Update);
    }
}
