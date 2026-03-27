use std::fs;
use std::io::BufReader;
use std::path::Path;
use std::process::{Command, Stdio};

use serde::{Deserialize, Serialize};

use crate::output::BlockWriter;
use crate::provider::{
    ApplyResult, BoxError, DynResource, FuncSignature, PlanAction, PlanResult, Provider, Resource, ResourceKind,
};
use crate::value::Value;

/// Typed inputs for an exec block, deserialized from the block's fields.
#[derive(Debug, Deserialize)]
pub struct ExecInputs {
    pub command: String,
    #[serde(deserialize_with = "string_or_vec")]
    pub output: Vec<String>,
    #[serde(default)]
    pub inputs: Vec<String>,
}

/// Typed outputs for an exec block, serialized into the scope for downstream blocks.
#[derive(Debug, Serialize)]
pub struct ExecOutputs {
    /// First output path (convenience for single-output blocks).
    pub path: String,
    /// All output paths.
    pub paths: Vec<String>,
}

/// State persisted between runs for an exec block.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExecState {
    pub command: String,
    pub output: Vec<String>,
}

/// Deserialize a field that can be either a single string or a list of strings.
fn string_or_vec<'de, D>(deserializer: D) -> Result<Vec<String>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    use serde::de;

    struct StringOrVec;

    impl<'de> de::Visitor<'de> for StringOrVec {
        type Value = Vec<String>;

        fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
            f.write_str("a string or list of strings")
        }

        fn visit_str<E: de::Error>(self, v: &str) -> Result<Vec<String>, E> {
            Ok(vec![v.to_owned()])
        }

        fn visit_seq<A: de::SeqAccess<'de>>(self, mut seq: A) -> Result<Vec<String>, A::Error> {
            let mut v = Vec::new();
            while let Some(s) = seq.next_element()? {
                v.push(s);
            }
            Ok(v)
        }
    }

    deserializer.deserialize_any(StringOrVec)
}

pub struct ExecProvider;

impl Provider for ExecProvider {
    fn name(&self) -> &str {
        "exec"
    }

    fn resources(&self) -> Vec<Box<dyn DynResource>> {
        vec![Box::new(ExecResource)]
    }

    fn functions(&self) -> Vec<FuncSignature> {
        vec![]
    }

    fn call_function(&self, name: &str, _args: &[Value]) -> Result<Value, BoxError> {
        Err(format!("exec provider has no function '{name}'").into())
    }
}

struct ExecResource;

impl Resource for ExecResource {
    type State = ExecState;
    type Inputs = ExecInputs;
    type Outputs = ExecOutputs;

    fn name(&self) -> &str {
        "exec"
    }

    fn kind(&self) -> ResourceKind {
        ResourceKind::Build
    }

    fn resolve(&self, inputs: &ExecInputs) -> Result<crate::provider::ResolvedFiles, BoxError> {
        let mut input_files = Vec::new();
        for pattern in &inputs.inputs {
            for entry in glob::glob(pattern).map_err(|e| format!("invalid glob '{pattern}': {e}"))? {
                let path = entry.map_err(|e| format!("glob error: {e}"))?;
                if path.is_file() {
                    input_files.push(path);
                }
            }
        }
        Ok(crate::provider::ResolvedFiles {
            inputs: input_files,
            outputs: inputs.output.iter().map(|o| Path::new(o).to_path_buf()).collect(),
        })
    }

    fn plan(&self, inputs: &ExecInputs, prior_state: Option<&ExecState>) -> Result<PlanResult, BoxError> {
        let Some(prior) = prior_state else {
            return Ok(PlanResult {
                action: PlanAction::Create,
                description: inputs.command.clone(),
            });
        };

        if prior.command != inputs.command {
            return Ok(PlanResult {
                action: PlanAction::Update,
                description: inputs.command.clone(),
            });
        }

        Ok(PlanResult {
            action: PlanAction::None,
            description: "no changes".into(),
        })
    }

    fn apply(
        &self,
        inputs: &ExecInputs,
        _prior_state: Option<&ExecState>,
        writer: &BlockWriter,
    ) -> Result<ApplyResult<ExecState, ExecOutputs>, BoxError> {
        for output in &inputs.output {
            let output_path = Path::new(output);
            if output.ends_with('/') {
                fs::create_dir_all(output_path)?;
            } else if let Some(parent) = output_path.parent()
                && !parent.as_os_str().is_empty()
            {
                fs::create_dir_all(parent)?;
            }
        }

        let mut child = Command::new("sh")
            .arg("-c")
            .arg(&inputs.command)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| format!("failed to execute command: {e}"))?;

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

        let status = child.wait().map_err(|e| format!("failed to wait for command: {e}"))?;
        if !status.success() {
            return Err(format!("command exited with {status}").into());
        }

        Ok(ApplyResult {
            outputs: ExecOutputs {
                path: inputs.output.first().cloned().unwrap_or_default(),
                paths: inputs.output.clone(),
            },
            state: Some(ExecState {
                command: inputs.command.clone(),
                output: inputs.output.clone(),
            }),
        })
    }

    fn destroy(&self, prior_state: &ExecState, writer: &BlockWriter) -> Result<(), BoxError> {
        for output in &prior_state.output {
            let path = Path::new(output);
            if path.is_dir() {
                writer.line(&format!("rm -rf {output}"));
                fs::remove_dir_all(path).ok();
            } else if path.is_file() {
                writer.line(&format!("rm {output}"));
                fs::remove_file(path).ok();
            }
        }
        Ok(())
    }

    fn refresh(&self, prior_state: &ExecState) -> Result<ApplyResult<ExecState, ExecOutputs>, BoxError> {
        Ok(ApplyResult {
            outputs: ExecOutputs {
                path: prior_state.output.first().cloned().unwrap_or_default(),
                paths: prior_state.output.clone(),
            },
            state: Some(prior_state.clone()),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::value::Map;
    use std::fs;
    use std::path::PathBuf;

    #[test]
    fn resolve_expands_globs() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("test.txt");
        fs::write(&file, "hello").unwrap();

        let inputs = ExecInputs {
            command: "echo".into(),
            output: vec!["out".into()],
            inputs: vec![dir.path().join("*.txt").to_string_lossy().into_owned()],
        };

        let resource = ExecResource;
        let result = Resource::resolve(&resource, &inputs).unwrap();
        assert_eq!(result.inputs.len(), 1);
        assert_eq!(result.inputs[0], file);
        assert_eq!(result.outputs, vec![PathBuf::from("out")]);
    }

    #[test]
    fn plan_create_when_no_prior_state() {
        let inputs = ExecInputs {
            command: "echo hi".into(),
            output: vec!["out".into()],
            inputs: vec![],
        };
        let resource = ExecResource;
        let result = Resource::plan(&resource, &inputs, None).unwrap();
        assert_eq!(result.action, PlanAction::Create);
    }

    #[test]
    fn plan_none_when_unchanged() {
        let inputs = ExecInputs {
            command: "echo hi".into(),
            output: vec!["out".into()],
            inputs: vec![],
        };
        let prior = ExecState {
            command: "echo hi".into(),
            output: vec!["out/".into()],
        };
        let resource = ExecResource;
        let result = Resource::plan(&resource, &inputs, Some(&prior)).unwrap();
        assert_eq!(result.action, PlanAction::None);
    }

    #[test]
    fn plan_update_when_command_changed() {
        let inputs = ExecInputs {
            command: "echo bye".into(),
            output: vec!["out".into()],
            inputs: vec![],
        };
        let prior = ExecState {
            command: "echo hi".into(),
            output: vec!["out/".into()],
        };
        let resource = ExecResource;
        let result = Resource::plan(&resource, &inputs, Some(&prior)).unwrap();
        assert_eq!(result.action, PlanAction::Update);
    }

    #[test]
    fn apply_runs_command() {
        let dir = tempfile::tempdir().unwrap();
        let output = dir.path().join("result.txt");
        let inputs = ExecInputs {
            command: format!("echo hello > {}", output.display()),
            output: vec![output.to_string_lossy().into_owned()],
            inputs: vec![],
        };
        let resource = ExecResource;
        let out = crate::output::Output::new(&[]);
        let writer = out.writer("test");
        let result = Resource::apply(&resource, &inputs, None, &writer).unwrap();
        assert!(result.state.is_some());
        assert!(output.exists());
        assert_eq!(fs::read_to_string(&output).unwrap().trim(), "hello");
    }

    #[test]
    fn apply_fails_on_bad_command() {
        let inputs = ExecInputs {
            command: "false".into(),
            output: vec!["/dev/null".into()],
            inputs: vec![],
        };
        let resource = ExecResource;
        let out = crate::output::Output::new(&[]);
        let writer = out.writer("test");
        assert!(Resource::apply(&resource, &inputs, None, &writer).is_err());
    }

    #[test]
    fn destroy_removes_output_dir() {
        let dir = tempfile::tempdir().unwrap();
        let output = dir.path().join("build_output");
        fs::create_dir_all(&output).unwrap();
        fs::write(output.join("file.txt"), "data").unwrap();

        let state = ExecState {
            command: "echo hi".into(),
            output: vec![output.to_string_lossy().into_owned()],
        };

        let resource = ExecResource;
        let out = crate::output::Output::new(&[]);
        let writer = out.writer("test");
        Resource::destroy(&resource, &state, &writer).unwrap();
        assert!(!output.exists());
    }

    #[test]
    fn provider_registration() {
        let provider = ExecProvider;
        assert_eq!(provider.name(), "exec");
        let resources = provider.resources();
        assert_eq!(resources.len(), 1);
        assert_eq!(resources[0].name(), "exec");
    }

    #[test]
    fn dyn_resource_deserializes_inputs() {
        let dir = tempfile::tempdir().unwrap();
        let output = dir.path().join("result.txt");
        let mut inputs = Map::new();
        inputs.insert(
            "command".into(),
            Value::Str(format!("echo hello > {}", output.display())),
        );
        inputs.insert("output".into(), Value::Str(output.to_string_lossy().into_owned()));

        let resource: Box<dyn DynResource> = Box::new(ExecResource);
        let out = crate::output::Output::new(&[]);
        let writer = out.writer("test");
        let result = resource.apply(&inputs, None, &writer).unwrap();
        assert!(result.state.is_some());
        assert_eq!(
            result.outputs.get("path").and_then(|v| v.as_str()),
            Some(output.to_string_lossy().as_ref())
        );
    }
}
