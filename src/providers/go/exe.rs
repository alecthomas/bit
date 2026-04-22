use std::collections::BTreeMap;
use std::fs;
use std::io::BufReader;
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};

use serde::{Deserialize, Serialize};

use crate::file_tracker::FileTracker;
use crate::output::BlockWriter;
use crate::provider::{ApplyResult, BoxError, PlanAction, PlanResult, Resource, ResourceKind};
use crate::sha256::SHA256;

use super::GoEnv;

/// Build a Go binary
#[derive(Debug, Deserialize, bit_derive::Schema)]
pub struct GoExeInputs {
    /// Go package to build (e.g. "./cmd/myapp")
    pub package: String,
    /// Output binary path (defaults to package base name)
    #[serde(default)]
    pub output: Option<String>,
    /// Extra flags passed to go build
    #[serde(default)]
    pub flags: Vec<String>,
    #[serde(flatten)]
    pub env: GoEnv,
}

/// Outputs from a `go.exe` block.
#[derive(Debug, Serialize, bit_derive::Schema)]
pub struct GoExeOutputs {
    /// Path to the built binary
    pub path: String,
}

/// Persisted state for a `go.exe` block.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GoExeState {
    pub package: String,
    pub output: String,
    pub flags: Vec<String>,
    #[serde(flatten)]
    pub env: GoEnv,
}

pub struct GoExeResource {
    tracker: Arc<Mutex<FileTracker>>,
}

impl GoExeResource {
    pub fn new(tracker: Arc<Mutex<FileTracker>>) -> Self {
        Self { tracker }
    }

    fn output_path(inputs: &GoExeInputs) -> String {
        inputs.output.clone().unwrap_or_else(|| {
            let base = inputs.package.rsplit('/').next().unwrap_or(&inputs.package);
            base.to_owned()
        })
    }
}

impl Resource for GoExeResource {
    type State = GoExeState;
    type Inputs = GoExeInputs;
    type Outputs = GoExeOutputs;

    fn name(&self) -> &str {
        "exe"
    }

    fn kind(&self) -> ResourceKind {
        ResourceKind::Build
    }

    fn resolve(&self, inputs: &GoExeInputs) -> Result<BTreeMap<String, SHA256>, BoxError> {
        let mut tracker = self.tracker.lock().expect("tracker lock poisoned");
        let mut files = super::resolve_go_inputs(&inputs.package, false, &mut tracker)?;
        let output = GoExeResource::output_path(inputs);
        let output_path = Path::new(&output);
        if output_path.exists() {
            let hash = tracker.hash_file(output_path)?;
            files.insert(output, hash);
        }
        Ok(files)
    }

    fn plan(&self, inputs: &GoExeInputs, prior_state: Option<&GoExeState>) -> Result<PlanResult, BoxError> {
        let output = GoExeResource::output_path(inputs);
        let description = format!("go build -o {output} {}", inputs.package);

        let Some(prior) = prior_state else {
            return Ok(PlanResult {
                action: PlanAction::Create,
                description,
                reason: None,
            });
        };

        let action = if prior.package != inputs.package
            || prior.output != output
            || prior.flags != inputs.flags
            || prior.env != inputs.env
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
        inputs: &GoExeInputs,
        _prior_state: Option<&GoExeState>,
        writer: &BlockWriter,
    ) -> Result<ApplyResult<GoExeState, GoExeOutputs>, BoxError> {
        let output = GoExeResource::output_path(inputs);

        if let Some(parent) = Path::new(&output).parent()
            && !parent.as_os_str().is_empty()
        {
            fs::create_dir_all(parent)?;
        }

        let mut args = vec!["build".to_owned(), "-o".to_owned(), output.clone()];
        args.extend(inputs.flags.iter().cloned());
        args.push(inputs.package.clone());

        let mut cmd = Command::new("go");
        cmd.args(&args).stdout(Stdio::piped()).stderr(Stdio::piped());
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
            outputs: GoExeOutputs { path: output.clone() },
            state: Some(GoExeState {
                package: inputs.package.clone(),
                output,
                flags: inputs.flags.clone(),
                env: inputs.env.clone(),
            }),
        })
    }

    fn destroy(&self, prior_state: &GoExeState, writer: &BlockWriter) -> Result<(), BoxError> {
        use crate::output::Event;
        let path = Path::new(&prior_state.output);
        if path.is_file() {
            writer.event(Event::Starting, &format!("rm {}", prior_state.output));
            fs::remove_file(path).ok();
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_resource() -> GoExeResource {
        GoExeResource::new(Arc::new(Mutex::new(FileTracker::default())))
    }

    #[test]
    fn resource_kind_is_build() {
        let resource = test_resource();
        assert_eq!(Resource::kind(&resource), ResourceKind::Build);
    }

    #[test]
    fn plan_create_when_no_prior_state() {
        let inputs = GoExeInputs {
            package: "./cmd/app".into(),
            output: Some("bin/app".into()),
            flags: vec![],
            env: GoEnv::default(),
        };
        let resource = test_resource();
        let result = Resource::plan(&resource, &inputs, None).unwrap();
        assert_eq!(result.action, PlanAction::Create);
    }

    #[test]
    fn plan_none_when_unchanged() {
        let inputs = GoExeInputs {
            package: "./cmd/app".into(),
            output: Some("bin/app".into()),
            flags: vec![],
            env: GoEnv::default(),
        };
        let prior = GoExeState {
            package: "./cmd/app".into(),
            output: "bin/app".into(),
            flags: vec![],
            env: GoEnv::default(),
        };
        let resource = test_resource();
        let result = Resource::plan(&resource, &inputs, Some(&prior)).unwrap();
        assert_eq!(result.action, PlanAction::None);
    }

    #[test]
    fn plan_update_when_package_changed() {
        let inputs = GoExeInputs {
            package: "./cmd/other".into(),
            output: Some("bin/app".into()),
            flags: vec![],
            env: GoEnv::default(),
        };
        let prior = GoExeState {
            package: "./cmd/app".into(),
            output: "bin/app".into(),
            flags: vec![],
            env: GoEnv::default(),
        };
        let resource = test_resource();
        let result = Resource::plan(&resource, &inputs, Some(&prior)).unwrap();
        assert_eq!(result.action, PlanAction::Update);
    }

    #[test]
    fn plan_update_when_flags_changed() {
        let inputs = GoExeInputs {
            package: "./cmd/app".into(),
            output: Some("bin/app".into()),
            flags: vec!["-ldflags=-s".into()],
            env: GoEnv::default(),
        };
        let prior = GoExeState {
            package: "./cmd/app".into(),
            output: "bin/app".into(),
            flags: vec![],
            env: GoEnv::default(),
        };
        let resource = test_resource();
        let result = Resource::plan(&resource, &inputs, Some(&prior)).unwrap();
        assert_eq!(result.action, PlanAction::Update);
    }

    #[test]
    fn plan_update_when_env_changed() {
        let inputs = GoExeInputs {
            package: "./cmd/app".into(),
            output: Some("bin/app".into()),
            flags: vec![],
            env: GoEnv {
                goos: Some("linux".into()),
                goarch: Some("arm64".into()),
                cgo: Some(false),
            },
        };
        let prior = GoExeState {
            package: "./cmd/app".into(),
            output: "bin/app".into(),
            flags: vec![],
            env: GoEnv::default(),
        };
        let resource = test_resource();
        let result = Resource::plan(&resource, &inputs, Some(&prior)).unwrap();
        assert_eq!(result.action, PlanAction::Update);
    }

    #[test]
    fn output_path_default() {
        let inputs = GoExeInputs {
            package: "./cmd/foo".into(),
            output: None,
            flags: vec![],
            env: GoEnv::default(),
        };
        assert_eq!(GoExeResource::output_path(&inputs), "foo");
    }

    #[test]
    fn output_path_explicit() {
        let inputs = GoExeInputs {
            package: "./cmd/foo".into(),
            output: Some("bin/foo".into()),
            flags: vec![],
            env: GoEnv::default(),
        };
        assert_eq!(GoExeResource::output_path(&inputs), "bin/foo");
    }

    #[test]
    fn destroy_removes_output() {
        let dir = tempfile::tempdir().unwrap();
        let output = dir.path().join("mybin");
        fs::write(&output, "binary").unwrap();

        let state = GoExeState {
            package: "./cmd/app".into(),
            output: output.to_string_lossy().into_owned(),
            flags: vec![],
            env: GoEnv::default(),
        };

        let resource = test_resource();
        let out = crate::output::Output::new(&[]);
        let writer = out.writer("test");
        Resource::destroy(&resource, &state, &writer).unwrap();
        assert!(!output.exists());
    }
}
