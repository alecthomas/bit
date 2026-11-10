use crate::process::CommandExt;
use std::collections::BTreeMap;
use std::io::BufReader;
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::Arc;

use serde::{Deserialize, Serialize};

use crate::file_tracker::FileTracker;
use crate::output::TargetWriter;
use crate::provider::{
    Ambient, ApplyResult, BoxError, CachePolicy, Changes, PlanAction, PlanResult, Resource, ResourceKind, Target,
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
    /// Extra flags passed to go generate. The files `-modfile`, `-overlay` and `-pgo` name are tracked as inputs. go runs in the directory `-C` names.
    #[serde(default)]
    pub flags: Vec<String>,
    /// Input file glob patterns, relative to `dir` (in addition to Go sources)
    #[serde(default)]
    pub inputs: Vec<String>,
    /// Output file paths produced by generate commands, relative to `dir`
    #[serde(default)]
    pub outputs: Vec<String>,
    #[serde(flatten)]
    pub go_env: GoEnv,
}

/// Outputs from a `go.generate` target (none meaningful).
#[derive(Debug, Serialize, bit_derive::Schema)]
pub struct GoGenerateOutputs {}

/// Persisted state for a `go.generate` target.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GoGenerateState {
    /// A string in state written before `package` took a list.
    #[serde(deserialize_with = "crate::providers::string_or_vec")]
    pub package: Vec<String>,
    pub flags: Vec<String>,
    #[serde(default)]
    pub outputs: Vec<String>,
    #[serde(flatten)]
    pub go_env: GoEnv,
}

/// The arguments to `go` that run the target's generate directives, in
/// [`go_dir`].
fn generate_args(inputs: &GoGenerateInputs) -> Vec<String> {
    let mut args = vec!["generate".to_owned()];
    args.extend(super::run_flags(&inputs.flags));
    args.extend(inputs.package.iter().cloned());
    args
}

/// The directory `go generate` runs in.
fn go_dir(inputs: &Target<GoGenerateInputs>) -> Option<String> {
    super::run_dir(inputs.dir.as_deref(), &inputs.flags)
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
    fn ambient(&self, inputs: &Target<GoGenerateInputs>) -> Vec<Ambient> {
        vec![super::toolchain(&inputs.go_env, go_dir(inputs).as_deref())]
    }

    fn resolve(&self, inputs: &Target<GoGenerateInputs>) -> Result<BTreeMap<String, SHA256>, BoxError> {
        // `go generate` also runs directives in `_test.go` files.
        super::resolve_go_inputs(
            &inputs.package,
            inputs.dir.as_deref(),
            &inputs.env,
            &inputs.flags,
            Tests::Sources,
            &inputs.inputs,
            &self.tracker,
        )
    }

    fn plan(
        &self,
        inputs: &Target<GoGenerateInputs>,
        prior_state: Option<&GoGenerateState>,
    ) -> Result<PlanResult, BoxError> {
        let description = super::display_command("go", &generate_args(inputs), go_dir(inputs).as_deref());

        let Some(prior) = prior_state else {
            return Ok(PlanResult {
                action: PlanAction::Create,
                description,
                reason: None,
            });
        };

        let action = if prior.package != inputs.package || prior.flags != inputs.flags || prior.go_env != inputs.go_env
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
        inputs: &Target<GoGenerateInputs>,
        _prior_state: Option<&GoGenerateState>,
        _changes: &Changes,
        writer: &TargetWriter,
    ) -> Result<ApplyResult<GoGenerateState, GoGenerateOutputs>, BoxError> {
        let args = generate_args(inputs);

        let mut cmd = Command::new("go");
        cmd.args(&args).stdout(Stdio::piped()).stderr(Stdio::piped());
        if let Some(dir) = go_dir(inputs) {
            cmd.current_dir(dir);
        }
        cmd.envs(&inputs.env);
        inputs.go_env.apply_to(&mut cmd);

        let mut child = cmd
            .logged_spawn()
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
                go_env: inputs.go_env.clone(),
            }),
        })
    }

    fn destroy(&self, prior_state: &Target<GoGenerateState>, writer: &TargetWriter) -> Result<(), BoxError> {
        for output in &prior_state.outputs {
            crate::providers::remove_path(Path::new(&prior_state.path(output)), writer)?;
        }
        Ok(())
    }

    /// `go generate` runs whatever the `//go:generate` directives in the
    /// sources name. Those tools are not part of the Go toolchain
    /// fingerprint, so the action key cannot describe what produced the
    /// generated files.
    fn cache_policy(&self, _inputs: &Target<GoGenerateInputs>) -> CachePolicy {
        CachePolicy::Local
    }

    fn output_keys(&self, inputs: &Target<GoGenerateInputs>) -> Vec<String> {
        inputs.outputs.iter().map(|output| inputs.path(output)).collect()
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
        let inputs = Target::new(
            Some(root.to_string_lossy().into_owned()),
            GoGenerateInputs {
                package: vec!["./...".into()],
                flags: vec![],
                inputs: vec![],
                outputs: vec![],
                go_env: GoEnv::default(),
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
        let inputs = Target::new(
            None,
            GoGenerateInputs {
                package: vec!["./...".into()],
                flags: vec![],
                inputs: vec![],
                outputs: vec![],
                go_env: GoEnv::default(),
            },
        );
        let result = Resource::plan(&test_resource(), &inputs, None).unwrap();
        assert_eq!(result.action, PlanAction::Create);
    }

    #[test]
    fn plan_none_when_unchanged() {
        let inputs = Target::new(
            None,
            GoGenerateInputs {
                package: vec!["./...".into()],
                flags: vec![],
                inputs: vec![],
                outputs: vec![],
                go_env: GoEnv::default(),
            },
        );
        let prior = GoGenerateState {
            package: vec!["./...".into()],
            flags: vec![],
            outputs: vec![],
            go_env: GoEnv::default(),
        };
        let result = Resource::plan(&test_resource(), &inputs, Some(&prior)).unwrap();
        assert_eq!(result.action, PlanAction::None);
    }

    #[test]
    fn plan_update_when_flags_changed() {
        let inputs = Target::new(
            None,
            GoGenerateInputs {
                package: vec!["./...".into()],
                flags: vec!["-v".into()],
                inputs: vec![],
                outputs: vec![],
                go_env: GoEnv::default(),
            },
        );
        let prior = GoGenerateState {
            package: vec!["./...".into()],
            flags: vec![],
            outputs: vec![],
            go_env: GoEnv::default(),
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
            go_env: GoEnv::default(),
        };

        let output = crate::output::Output::new(&[]);
        Resource::destroy(&test_resource(), &Target::new(None, state), &output.writer("generate")).unwrap();

        assert!(!file.exists());
        assert!(!output_dir.exists());
    }
}
