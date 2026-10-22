use std::collections::BTreeMap;
use std::io::BufReader;
use std::path::Path;
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

/// Run go generate
#[derive(Debug, Deserialize, bit_derive::Schema)]
pub struct GoGenerateInputs {
    /// Go package pattern or list of patterns (e.g. "./...")
    #[serde(deserialize_with = "super::package_patterns")]
    #[schema(typ = crate::schema::string_or_list())]
    pub package: Vec<String>,
    /// Extra flags passed to go generate
    #[serde(default)]
    pub flags: Vec<String>,
    /// Input file glob patterns (in addition to Go sources)
    #[serde(default)]
    pub inputs: Vec<String>,
    /// Output file paths produced by generate commands
    #[serde(default)]
    pub outputs: Vec<String>,
    #[serde(flatten)]
    pub env: GoEnv,
}

/// Outputs from a `go.generate` block (none meaningful).
#[derive(Debug, Serialize, bit_derive::Schema)]
pub struct GoGenerateOutputs {}

/// Persisted state for a `go.generate` block.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GoGenerateState {
    /// A string in state written before `package` took a list.
    #[serde(deserialize_with = "crate::providers::string_or_vec")]
    pub package: Vec<String>,
    pub flags: Vec<String>,
    #[serde(default)]
    pub outputs: Vec<String>,
    #[serde(flatten)]
    pub env: GoEnv,
}

/// The arguments to `go` that run the block's generate directives.
fn generate_args(inputs: &GoGenerateInputs) -> Vec<String> {
    let mut args = vec!["generate".to_owned()];
    args.extend(inputs.flags.iter().cloned());
    args.extend(inputs.package.iter().cloned());
    args
}

pub struct GoGenerateResource {
    tracker: Arc<FileTracker>,
}

impl GoGenerateResource {
    pub fn new(tracker: Arc<FileTracker>) -> Self {
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

    /// Only the go command: the generators its directives run are unknown.
    fn ambient(&self, inputs: &Block<GoGenerateInputs>) -> Vec<Ambient> {
        vec![super::toolchain(&inputs.env, inputs.dir.as_deref())]
    }

    fn resolve(&self, inputs: &Block<GoGenerateInputs>) -> Result<BTreeMap<String, SHA256>, BoxError> {
        let tracker = &*self.tracker;
        let dir = inputs.dir.as_deref().map(Path::new);
        // `go generate` also runs directives in `_test.go` files.
        super::resolve_go_inputs(&inputs.package, dir, Tests::Sources, &inputs.inputs, tracker)
    }

    fn plan(
        &self,
        inputs: &Block<GoGenerateInputs>,
        prior_state: Option<&GoGenerateState>,
    ) -> Result<PlanResult, BoxError> {
        let description = super::display_command("go", &generate_args(inputs), inputs.dir.as_deref());

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
        inputs: &Block<GoGenerateInputs>,
        _prior_state: Option<&GoGenerateState>,
        _changes: &Changes,
        writer: &BlockWriter,
    ) -> Result<ApplyResult<GoGenerateState, GoGenerateOutputs>, BoxError> {
        let args = generate_args(inputs);

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
                outputs: inputs.outputs.clone(),
                env: inputs.env.clone(),
            }),
        })
    }

    fn destroy(&self, prior_state: &Block<GoGenerateState>, writer: &BlockWriter) -> Result<(), BoxError> {
        for output in &prior_state.outputs {
            crate::providers::remove_path(Path::new(output), writer)?;
        }
        Ok(())
    }

    /// `go generate` runs whatever the `//go:generate` directives in the
    /// sources name. Those tools are not part of the Go toolchain
    /// fingerprint, so the action key cannot describe what produced the
    /// generated files.
    fn cache_policy(&self, _inputs: &Block<GoGenerateInputs>) -> CachePolicy {
        CachePolicy::Local
    }

    fn output_keys(&self, inputs: &Block<GoGenerateInputs>) -> Vec<String> {
        inputs.outputs.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_resource() -> GoGenerateResource {
        GoGenerateResource::new(Arc::new(FileTracker::default()))
    }

    #[test]
    fn resolve_tracks_test_files() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::write(root.join("go.mod"), "module example.com/test\n").unwrap();
        std::fs::write(root.join("lib.go"), "package lib\n").unwrap();
        std::fs::write(root.join("lib_test.go"), "package lib\n\n//go:generate echo test\n").unwrap();
        let inputs = Block::new(
            Some(root.to_string_lossy().into_owned()),
            GoGenerateInputs {
                package: vec!["./...".into()],
                flags: vec![],
                inputs: vec![],
                outputs: vec![],
                env: GoEnv::default(),
            },
        );
        let resolved = Resource::resolve(&test_resource(), &inputs).unwrap();
        assert!(resolved.keys().any(|p| p.ends_with("lib_test.go")), "{resolved:?}");
    }

    #[test]
    fn resource_kind_is_build() {
        assert_eq!(Resource::kind(&test_resource()), ResourceKind::Build);
    }

    #[test]
    fn plan_create_when_no_prior_state() {
        let inputs = Block::new(
            None,
            GoGenerateInputs {
                package: vec!["./...".into()],
                flags: vec![],
                inputs: vec![],
                outputs: vec![],
                env: GoEnv::default(),
            },
        );
        let result = Resource::plan(&test_resource(), &inputs, None).unwrap();
        assert_eq!(result.action, PlanAction::Create);
    }

    #[test]
    fn plan_none_when_unchanged() {
        let inputs = Block::new(
            None,
            GoGenerateInputs {
                package: vec!["./...".into()],
                flags: vec![],
                inputs: vec![],
                outputs: vec![],
                env: GoEnv::default(),
            },
        );
        let prior = GoGenerateState {
            package: vec!["./...".into()],
            flags: vec![],
            outputs: vec![],
            env: GoEnv::default(),
        };
        let result = Resource::plan(&test_resource(), &inputs, Some(&prior)).unwrap();
        assert_eq!(result.action, PlanAction::None);
    }

    #[test]
    fn plan_update_when_flags_changed() {
        let inputs = Block::new(
            None,
            GoGenerateInputs {
                package: vec!["./...".into()],
                flags: vec!["-v".into()],
                inputs: vec![],
                outputs: vec![],
                env: GoEnv::default(),
            },
        );
        let prior = GoGenerateState {
            package: vec!["./...".into()],
            flags: vec![],
            outputs: vec![],
            env: GoEnv::default(),
        };
        let result = Resource::plan(&test_resource(), &inputs, Some(&prior)).unwrap();
        assert_eq!(result.action, PlanAction::Update);
    }

    #[test]
    fn destroy_removes_declared_outputs() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("generated.go");
        let output_dir = dir.path().join("generated");
        std::fs::write(&file, "package generated").unwrap();
        std::fs::create_dir(&output_dir).unwrap();
        std::fs::write(output_dir.join("data.go"), "package generated").unwrap();
        let state = GoGenerateState {
            package: vec!["./...".into()],
            flags: vec![],
            outputs: vec![
                file.to_string_lossy().into_owned(),
                output_dir.to_string_lossy().into_owned(),
            ],
            env: GoEnv::default(),
        };

        let output = crate::output::Output::new(&[]);
        Resource::destroy(&test_resource(), &Block::new(None, state), &output.writer("generate")).unwrap();

        assert!(!file.exists());
        assert!(!output_dir.exists());
    }
}
