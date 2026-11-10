use crate::process::CommandExt;
use std::collections::BTreeMap;
use std::io::BufReader;
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

/// Compile Go packages without producing a binary
#[derive(Debug, Deserialize, bit_derive::Schema)]
pub struct GoBuildInputs {
    /// Go package pattern or list of patterns (e.g. "./...")
    #[serde(deserialize_with = "super::package_patterns")]
    #[schema(typ = crate::schema::string_or_list())]
    pub package: Vec<String>,
    /// Extra flags passed to go build. The files `-o` writes are tracked as outputs, and the files `-modfile`, `-overlay` and `-pgo` name as inputs. go runs in the directory `-C` names.
    #[serde(default)]
    pub flags: Vec<String>,
    /// Input file glob patterns, relative to `dir` (in addition to Go sources)
    #[serde(default)]
    pub inputs: Vec<String>,
    #[serde(flatten)]
    pub go_env: GoEnv,
}

/// Outputs from a `go.build` target (none meaningful).
#[derive(Debug, Serialize, bit_derive::Schema)]
pub struct GoBuildOutputs {}

/// Persisted state for a `go.build` target.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GoBuildState {
    /// A string in state written before `package` took a list.
    #[serde(deserialize_with = "crate::providers::string_or_vec")]
    pub package: Vec<String>,
    pub flags: Vec<String>,
    #[serde(flatten)]
    pub go_env: GoEnv,
}

/// The arguments to `go` that compile the target's packages, in
/// [`super::run_dir`].
fn build_args(inputs: &GoBuildInputs) -> Vec<String> {
    let mut args = vec!["build".to_owned()];
    args.extend(super::run_flags(&inputs.flags));
    args.extend(inputs.package.iter().cloned());
    args
}

/// The directory `go build` runs in.
fn go_dir(inputs: &Target<GoBuildInputs>) -> Option<String> {
    super::run_dir(inputs.dir.as_deref(), &inputs.flags)
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

    fn ambient(&self, inputs: &Target<GoBuildInputs>) -> Vec<Ambient> {
        vec![super::toolchain(&inputs.go_env, go_dir(inputs).as_deref())]
    }

    fn resolve(&self, inputs: &Target<GoBuildInputs>) -> Result<BTreeMap<String, SHA256>, BoxError> {
        super::resolve_go_inputs(
            &inputs.package,
            inputs.dir.as_deref(),
            &inputs.env,
            &inputs.flags,
            Tests::Excluded,
            &inputs.inputs,
            &self.tracker,
        )
    }

    fn plan(&self, inputs: &Target<GoBuildInputs>, prior_state: Option<&GoBuildState>) -> Result<PlanResult, BoxError> {
        let description = super::display_command("go", &build_args(inputs), go_dir(inputs).as_deref());

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
        inputs: &Target<GoBuildInputs>,
        _prior_state: Option<&GoBuildState>,
        _changes: &Changes,
        writer: &TargetWriter,
    ) -> Result<ApplyResult<GoBuildState, GoBuildOutputs>, BoxError> {
        let args = build_args(inputs);

        let mut cmd = Command::new("go");
        cmd.args(&args).stdout(Stdio::piped()).stderr(Stdio::piped());
        if let Some(dir) = go_dir(inputs) {
            cmd.current_dir(dir);
        }
        cmd.envs(&inputs.env);
        inputs.go_env.apply_to(&mut cmd);

        let mut child = cmd
            .logged_spawn()
            .map_err(|e| format!("failed to execute `go build`: {e}"))?;

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
                go_env: inputs.go_env.clone(),
            }),
        })
    }

    fn destroy(&self, _prior_state: &Target<GoBuildState>, _writer: &TargetWriter) -> Result<(), BoxError> {
        Ok(())
    }

    fn cache_policy(&self, _inputs: &Target<GoBuildInputs>) -> CachePolicy {
        CachePolicy::Shared { version: 1 }
    }

    fn output_keys(&self, inputs: &Target<GoBuildInputs>) -> Vec<String> {
        super::build_outputs(go_dir(inputs).as_deref(), &inputs.flags)
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
        let inputs = Target::new(
            None,
            GoBuildInputs {
                package: vec!["./...".into()],
                flags: vec!["-race".into()],
                inputs: vec![],
                go_env: GoEnv::default(),
            },
        );
        let resource = test_resource();
        let result = Resource::plan(&resource, &inputs, None).unwrap();
        assert_eq!(result.action, PlanAction::Create);
        assert_eq!(result.description, "go build -race ./...");
    }

    #[test]
    fn output_flag_names_outputs_and_chdir_runs_go_elsewhere() {
        let inputs = Target::new(
            Some("svc".into()),
            GoBuildInputs {
                package: vec!["./...".into()],
                flags: vec!["-C".into(), "mod".into(), "-o".into(), "bin/".into()],
                inputs: vec![],
                go_env: GoEnv::default(),
            },
        );
        assert_eq!(Resource::output_keys(&test_resource(), &inputs), ["svc/mod/bin/"]);
        assert_eq!(build_args(&inputs), ["build", "-o", "bin/", "./..."]);
        assert_eq!(go_dir(&inputs).as_deref(), Some("svc/mod"));
    }

    #[test]
    fn plan_none_when_unchanged() {
        let inputs = Target::new(
            None,
            GoBuildInputs {
                package: vec!["./...".into()],
                flags: vec![],
                inputs: vec![],
                go_env: GoEnv::default(),
            },
        );
        let prior = GoBuildState {
            package: vec!["./...".into()],
            flags: vec![],
            go_env: GoEnv::default(),
        };
        let resource = test_resource();
        let result = Resource::plan(&resource, &inputs, Some(&prior)).unwrap();
        assert_eq!(result.action, PlanAction::None);
    }
}
