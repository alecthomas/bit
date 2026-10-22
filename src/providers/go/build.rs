use std::collections::BTreeMap;
use std::io::BufReader;
use std::process::{Command, Stdio};
use std::sync::Arc;

use serde::{Deserialize, Serialize};

use crate::file_tracker::FileTracker;
use crate::output::BlockWriter;
use crate::provider::{
    Ambient, ApplyResult, Block, BoxError, CachePolicy, Changes, PlanAction, PlanResult, Resource, ResourceKind,
};
use crate::sha256::SHA256;

use super::GoEnv;
use super::scanner::Tests;

/// Compile Go packages without producing a binary
#[derive(Debug, Deserialize, bit_derive::Schema)]
pub struct GoBuildInputs {
    /// Go package pattern or list of patterns (e.g. "./...")
    #[serde(deserialize_with = "super::package_patterns")]
    #[schema(typ = crate::schema::string_or_list())]
    pub package: Vec<String>,
    /// Extra flags passed to go build
    #[serde(default)]
    pub flags: Vec<String>,
    /// Input file glob patterns, relative to `dir` (in addition to Go sources)
    #[serde(default)]
    pub inputs: Vec<String>,
    #[serde(flatten)]
    pub env: GoEnv,
}

/// Outputs from a `go.build` block (none meaningful).
#[derive(Debug, Serialize, bit_derive::Schema)]
pub struct GoBuildOutputs {}

/// Persisted state for a `go.build` block.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GoBuildState {
    /// A string in state written before `package` took a list.
    #[serde(deserialize_with = "crate::providers::string_or_vec")]
    pub package: Vec<String>,
    pub flags: Vec<String>,
    #[serde(flatten)]
    pub env: GoEnv,
}

/// The arguments to `go` that compile the block's packages.
fn build_args(inputs: &GoBuildInputs) -> Vec<String> {
    let mut args = vec!["build".to_owned()];
    args.extend(inputs.flags.iter().cloned());
    args.extend(inputs.package.iter().cloned());
    args
}

pub struct GoBuildResource {
    tracker: Arc<FileTracker>,
}

impl GoBuildResource {
    pub fn new(tracker: Arc<FileTracker>) -> Self {
        Self { tracker }
    }
}

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

    fn ambient(&self, inputs: &Block<GoBuildInputs>) -> Vec<Ambient> {
        vec![super::toolchain(&inputs.env, inputs.dir.as_deref())]
    }

    fn resolve(&self, inputs: &Block<GoBuildInputs>) -> Result<BTreeMap<String, SHA256>, BoxError> {
        let tracker = &*self.tracker;
        let dir = inputs.dir.as_deref().map(std::path::Path::new);
        super::resolve_go_inputs(&inputs.package, dir, Tests::Excluded, &inputs.inputs, tracker)
    }

    fn plan(&self, inputs: &Block<GoBuildInputs>, prior_state: Option<&GoBuildState>) -> Result<PlanResult, BoxError> {
        let description = super::display_command("go", &build_args(inputs), inputs.dir.as_deref());

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
        inputs: &Block<GoBuildInputs>,
        _prior_state: Option<&GoBuildState>,
        _changes: &Changes,
        writer: &BlockWriter,
    ) -> Result<ApplyResult<GoBuildState, GoBuildOutputs>, BoxError> {
        let args = build_args(inputs);

        let mut cmd = Command::new("go");
        cmd.args(&args).stdout(Stdio::piped()).stderr(Stdio::piped());
        if let Some(dir) = &inputs.dir {
            cmd.current_dir(dir);
        }
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

    fn destroy(&self, _prior_state: &Block<GoBuildState>, _writer: &BlockWriter) -> Result<(), BoxError> {
        Ok(())
    }

    fn cache_policy(&self, _inputs: &Block<GoBuildInputs>) -> CachePolicy {
        CachePolicy::Shared { version: 1 }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_resource() -> GoBuildResource {
        GoBuildResource::new(Arc::new(FileTracker::default()))
    }

    #[test]
    fn resource_kind_is_build() {
        let resource = test_resource();
        assert_eq!(Resource::kind(&resource), ResourceKind::Build);
    }

    #[test]
    fn plan_create_when_no_prior_state() {
        let inputs = Block::new(
            None,
            GoBuildInputs {
                package: vec!["./...".into()],
                flags: vec!["-race".into()],
                inputs: vec![],
                env: GoEnv::default(),
            },
        );
        let resource = test_resource();
        let result = Resource::plan(&resource, &inputs, None).unwrap();
        assert_eq!(result.action, PlanAction::Create);
        assert_eq!(result.description, "go build -race ./...");
    }

    #[test]
    fn plan_none_when_unchanged() {
        let inputs = Block::new(
            None,
            GoBuildInputs {
                package: vec!["./...".into()],
                flags: vec![],
                inputs: vec![],
                env: GoEnv::default(),
            },
        );
        let prior = GoBuildState {
            package: vec!["./...".into()],
            flags: vec![],
            env: GoEnv::default(),
        };
        let resource = test_resource();
        let result = Resource::plan(&resource, &inputs, Some(&prior)).unwrap();
        assert_eq!(result.action, PlanAction::None);
    }
}
