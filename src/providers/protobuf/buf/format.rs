use std::collections::BTreeMap;
use std::sync::Arc;

use serde::{Deserialize, Serialize};

use crate::file_tracker::FileTracker;
use crate::output::BlockWriter;
use crate::provider::{
    Ambient, ApplyResult, Block, BoxError, CachePolicy, Changes, PlanResult, Resource, ResourceKind,
};
use crate::sha256::SHA256;

use super::BufTarget;

/// Format Protobuf files with buf format
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, bit_derive::Schema)]
pub struct BufFormatInputs {
    #[serde(flatten)]
    pub target: BufTarget,
    /// Extra flags passed to buf format. A config file `--config` names is tracked as an input.
    #[serde(default)]
    pub flags: Vec<String>,
}

impl BufFormatInputs {
    /// `buf format <mode...> <flags...>`.
    fn args(&self, mode: &[&str]) -> Vec<String> {
        let mut args = vec!["format".to_owned()];
        args.extend(mode.iter().map(|arg| (*arg).to_owned()));
        args.extend(self.flags.iter().cloned());
        args
    }
}

/// Outputs from a `buf.format` block.
#[derive(Debug, Serialize, bit_derive::Schema)]
pub struct BufFormatOutputs {}

/// Outputs from a `buf.format-check` block.
#[derive(Debug, Serialize, bit_derive::Schema)]
pub struct BufFormatCheckOutputs {
    /// Whether all files are formatted
    pub passed: bool,
}

const WRITE: [&str; 1] = ["--write"];
/// `--exit-code` makes buf fail when a file would change; `--diff` shows how.
const CHECK: [&str; 2] = ["--diff", "--exit-code"];

// ── buf.format (build) ──────────────────────────────────────────────────

pub struct BufFormatResource {
    tracker: Arc<FileTracker>,
}

impl BufFormatResource {
    pub fn new(tracker: Arc<FileTracker>) -> Self {
        Self { tracker }
    }
}

impl Resource for BufFormatResource {
    type State = BufFormatInputs;
    type Inputs = BufFormatInputs;
    type Outputs = BufFormatOutputs;

    fn name(&self) -> &str {
        "format"
    }

    fn kind(&self) -> ResourceKind {
        ResourceKind::Build
    }

    fn resolve(&self, inputs: &Block<BufFormatInputs>) -> Result<BTreeMap<String, SHA256>, BoxError> {
        inputs
            .target
            .sources(inputs.dir.as_deref(), &inputs.flags)?
            .hash(&self.tracker)
    }

    fn ambient(&self, inputs: &Block<BufFormatInputs>) -> Vec<Ambient> {
        vec![super::buf(inputs.dir.as_deref())]
    }

    fn plan(
        &self,
        inputs: &Block<BufFormatInputs>,
        prior_state: Option<&BufFormatInputs>,
    ) -> Result<PlanResult, BoxError> {
        Ok(super::super::plan(
            inputs.target.describe(&inputs.args(&WRITE)),
            prior_state,
            inputs,
        ))
    }

    fn apply(
        &self,
        inputs: &Block<BufFormatInputs>,
        _prior_state: Option<&BufFormatInputs>,
        _changes: &Changes,
        writer: &BlockWriter,
    ) -> Result<ApplyResult<BufFormatInputs, BufFormatOutputs>, BoxError> {
        let status = inputs.target.run(inputs.dir.as_deref(), &inputs.args(&WRITE), writer)?;
        if !status.success() {
            return Err(format!("`buf format` exited with {status}").into());
        }
        Ok(ApplyResult {
            outputs: BufFormatOutputs {},
            state: Some(inputs.value.clone()),
        })
    }

    fn destroy(&self, _prior_state: &Block<BufFormatInputs>, _writer: &BlockWriter) -> Result<(), BoxError> {
        Ok(())
    }

    /// The buf executable is a tool, so the action key already describes
    /// the toolchain, but not a remote input's content.
    fn cache_policy(&self, inputs: &Block<BufFormatInputs>) -> CachePolicy {
        inputs.target.cache_policy(inputs.dir.as_deref())
    }
}

// ── buf.format-check (test) ─────────────────────────────────────────────

pub struct BufFormatCheckResource {
    tracker: Arc<FileTracker>,
}

impl BufFormatCheckResource {
    pub fn new(tracker: Arc<FileTracker>) -> Self {
        Self { tracker }
    }
}

impl Resource for BufFormatCheckResource {
    type State = BufFormatInputs;
    type Inputs = BufFormatInputs;
    type Outputs = BufFormatCheckOutputs;

    fn name(&self) -> &str {
        "format-check"
    }

    fn kind(&self) -> ResourceKind {
        ResourceKind::Test
    }

    fn resolve(&self, inputs: &Block<BufFormatInputs>) -> Result<BTreeMap<String, SHA256>, BoxError> {
        inputs
            .target
            .sources(inputs.dir.as_deref(), &inputs.flags)?
            .hash(&self.tracker)
    }

    fn ambient(&self, inputs: &Block<BufFormatInputs>) -> Vec<Ambient> {
        vec![super::buf(inputs.dir.as_deref())]
    }

    fn plan(
        &self,
        inputs: &Block<BufFormatInputs>,
        prior_state: Option<&BufFormatInputs>,
    ) -> Result<PlanResult, BoxError> {
        Ok(super::super::plan(
            inputs.target.describe(&inputs.args(&CHECK)),
            prior_state,
            inputs,
        ))
    }

    fn apply(
        &self,
        inputs: &Block<BufFormatInputs>,
        _prior_state: Option<&BufFormatInputs>,
        _changes: &Changes,
        writer: &BlockWriter,
    ) -> Result<ApplyResult<BufFormatInputs, BufFormatCheckOutputs>, BoxError> {
        let status = inputs.target.run(inputs.dir.as_deref(), &inputs.args(&CHECK), writer)?;
        Ok(ApplyResult {
            outputs: BufFormatCheckOutputs {
                passed: status.success(),
            },
            state: Some(inputs.value.clone()),
        })
    }

    fn destroy(&self, _prior_state: &Block<BufFormatInputs>, _writer: &BlockWriter) -> Result<(), BoxError> {
        Ok(())
    }

    fn cache_policy(&self, inputs: &Block<BufFormatInputs>) -> CachePolicy {
        inputs.target.cache_policy(inputs.dir.as_deref())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tracker() -> Arc<FileTracker> {
        Arc::new(FileTracker::default())
    }

    #[test]
    fn resource_kinds() {
        assert_eq!(Resource::kind(&BufFormatResource::new(tracker())), ResourceKind::Build);
        assert_eq!(
            Resource::kind(&BufFormatCheckResource::new(tracker())),
            ResourceKind::Test
        );
    }

    #[test]
    fn plan_descriptions() {
        let inputs: BufFormatInputs = serde_json::from_str(r#"{"input": "proto"}"#).unwrap();
        let inputs = Block::new(None, inputs);
        let format = Resource::plan(&BufFormatResource::new(tracker()), &inputs, None).unwrap();
        assert_eq!(format.description, "buf format --write proto");
        let check = Resource::plan(&BufFormatCheckResource::new(tracker()), &inputs, None).unwrap();
        assert_eq!(check.description, "buf format --diff --exit-code proto");
    }
}
