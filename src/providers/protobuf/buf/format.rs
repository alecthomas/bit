use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use serde::{Deserialize, Serialize};

use crate::file_tracker::FileTracker;
use crate::output::BlockWriter;
use crate::provider::{Ambient, ApplyResult, BoxError, CachePolicy, PlanResult, Resource, ResourceKind};
use crate::sha256::SHA256;

use super::BufTarget;

/// Format Protobuf files with buf format
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, bit_derive::Schema)]
pub struct BufFormatInputs {
    #[serde(flatten)]
    pub target: BufTarget,
    /// Extra flags passed to buf format
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
    tracker: Arc<Mutex<FileTracker>>,
}

impl BufFormatResource {
    pub fn new(tracker: Arc<Mutex<FileTracker>>) -> Self {
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

    fn resolve(&self, inputs: &BufFormatInputs) -> Result<BTreeMap<String, SHA256>, BoxError> {
        let mut tracker = self.tracker.lock().expect("tracker lock poisoned");
        inputs.target.resolve(&[], &mut tracker)
    }

    fn ambient(&self, inputs: &BufFormatInputs) -> Vec<Ambient> {
        vec![inputs.target.buf()]
    }

    fn plan(&self, inputs: &BufFormatInputs, prior_state: Option<&BufFormatInputs>) -> Result<PlanResult, BoxError> {
        Ok(super::super::plan(
            inputs.target.describe(&inputs.args(&WRITE)),
            prior_state,
            inputs,
        ))
    }

    fn apply(
        &self,
        inputs: &BufFormatInputs,
        _prior_state: Option<&BufFormatInputs>,
        writer: &BlockWriter,
    ) -> Result<ApplyResult<BufFormatInputs, BufFormatOutputs>, BoxError> {
        let status = inputs.target.run(&inputs.args(&WRITE), writer)?;
        if !status.success() {
            return Err(format!("`buf format` exited with {status}").into());
        }
        Ok(ApplyResult {
            outputs: BufFormatOutputs {},
            state: Some(inputs.clone()),
        })
    }

    fn destroy(&self, _prior_state: &BufFormatInputs, _writer: &BlockWriter) -> Result<(), BoxError> {
        Ok(())
    }

    /// The buf executable is a tool, so the action key already describes
    /// the toolchain.
    fn cache_policy(&self, _inputs: &BufFormatInputs) -> CachePolicy {
        CachePolicy::Shared { version: 1 }
    }
}

// ── buf.format-check (test) ─────────────────────────────────────────────

pub struct BufFormatCheckResource {
    tracker: Arc<Mutex<FileTracker>>,
}

impl BufFormatCheckResource {
    pub fn new(tracker: Arc<Mutex<FileTracker>>) -> Self {
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

    fn resolve(&self, inputs: &BufFormatInputs) -> Result<BTreeMap<String, SHA256>, BoxError> {
        let mut tracker = self.tracker.lock().expect("tracker lock poisoned");
        inputs.target.resolve(&[], &mut tracker)
    }

    fn ambient(&self, inputs: &BufFormatInputs) -> Vec<Ambient> {
        vec![inputs.target.buf()]
    }

    fn plan(&self, inputs: &BufFormatInputs, prior_state: Option<&BufFormatInputs>) -> Result<PlanResult, BoxError> {
        Ok(super::super::plan(
            inputs.target.describe(&inputs.args(&CHECK)),
            prior_state,
            inputs,
        ))
    }

    fn apply(
        &self,
        inputs: &BufFormatInputs,
        _prior_state: Option<&BufFormatInputs>,
        writer: &BlockWriter,
    ) -> Result<ApplyResult<BufFormatInputs, BufFormatCheckOutputs>, BoxError> {
        let status = inputs.target.run(&inputs.args(&CHECK), writer)?;
        Ok(ApplyResult {
            outputs: BufFormatCheckOutputs {
                passed: status.success(),
            },
            state: Some(inputs.clone()),
        })
    }

    fn destroy(&self, _prior_state: &BufFormatInputs, _writer: &BlockWriter) -> Result<(), BoxError> {
        Ok(())
    }

    fn cache_policy(&self, _inputs: &BufFormatInputs) -> CachePolicy {
        CachePolicy::Shared { version: 1 }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tracker() -> Arc<Mutex<FileTracker>> {
        Arc::new(Mutex::new(FileTracker::default()))
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
        let format = Resource::plan(&BufFormatResource::new(tracker()), &inputs, None).unwrap();
        assert_eq!(format.description, "buf format --write proto");
        let check = Resource::plan(&BufFormatCheckResource::new(tracker()), &inputs, None).unwrap();
        assert_eq!(check.description, "buf format --diff --exit-code proto");
    }
}
