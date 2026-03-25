use std::collections::HashMap;
use std::fs;
use std::io::BufReader;
use std::path::Path;
use std::process::{Command, Stdio};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::output::BlockWriter;
use crate::provider::{
    ApplyResult, BoxError, DynResource, FuncSignature, OutputSchema, PlanAction, PlanResult, Provider, ResolveResult,
    ResolvedInput, ResolvedPath, Resource, ResourceKind,
};
use crate::value::{Map, Type, Value};

/// State persisted between runs for an exec block.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExecState {
    pub command: String,
    pub output: String,
    pub input_hashes: HashMap<String, String>,
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

    fn name(&self) -> &str {
        "exec"
    }

    fn kind(&self) -> ResourceKind {
        ResourceKind::Build
    }

    fn outputs(&self) -> Vec<OutputSchema> {
        vec![OutputSchema {
            name: "path".into(),
            typ: Type::String,
        }]
    }

    /// Expand input globs to concrete files with content hashes.
    fn resolve(&self, inputs: &Map) -> Result<ResolveResult, BoxError> {
        let input_globs = extract_string_list(inputs, "inputs")?;
        let mut paths = Vec::new();
        let mut watches = Vec::new();

        for pattern in &input_globs {
            watches.push(pattern.clone());
            for entry in glob::glob(pattern).map_err(|e| format!("invalid glob '{pattern}': {e}"))? {
                let path = entry.map_err(|e| format!("glob error: {e}"))?;
                if path.is_file() {
                    let hash = hash_file(&path)?;
                    paths.push(ResolvedPath {
                        path: path.to_string_lossy().into_owned(),
                        content_hash: hash,
                    });
                }
            }
        }

        Ok(ResolveResult {
            inputs: vec![ResolvedInput {
                key: "inputs".into(),
                paths,
            }],
            watches,
            platform: vec![],
        })
    }

    fn plan(&self, inputs: &Map, prior_state: Option<&ExecState>) -> Result<PlanResult, BoxError> {
        let command = extract_string(inputs, "command")?;
        let Some(prior) = prior_state else {
            return Ok(PlanResult {
                action: PlanAction::Create,
                description: format!("run: {command}"),
            });
        };

        if prior.command != command {
            return Ok(PlanResult {
                action: PlanAction::Update,
                description: format!("run: {command}"),
            });
        }

        let current_hashes = hash_inputs(inputs);
        if current_hashes != prior.input_hashes {
            return Ok(PlanResult {
                action: PlanAction::Update,
                description: format!("run: {command}"),
            });
        }

        Ok(PlanResult {
            action: PlanAction::None,
            description: "no changes".into(),
        })
    }

    fn apply(
        &self,
        inputs: &Map,
        _prior_state: Option<&ExecState>,
        writer: &BlockWriter,
    ) -> Result<ApplyResult<ExecState>, BoxError> {
        let command = extract_string(inputs, "command")?;
        let output = extract_string(inputs, "output")?;

        let output_path = Path::new(&output);
        if output.ends_with('/') {
            fs::create_dir_all(output_path)?;
        } else if let Some(parent) = output_path.parent()
            && !parent.as_os_str().is_empty()
        {
            fs::create_dir_all(parent)?;
        }

        let mut child = Command::new("sh")
            .arg("-c")
            .arg(&command)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| format!("failed to execute command: {e}"))?;

        // Stream stdout and stderr through the block writer
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

        let input_hashes = hash_inputs(inputs);

        let mut outputs = Map::new();
        outputs.insert("path".into(), Value::Str(output.clone()));

        let state = ExecState {
            command,
            output,
            input_hashes,
        };

        Ok(ApplyResult {
            outputs,
            state: Some(state),
        })
    }

    fn destroy(&self, prior_state: &ExecState, writer: &BlockWriter) -> Result<(), BoxError> {
        let path = Path::new(&prior_state.output);
        if path.is_dir() {
            writer.line(&format!("rm -rf {}", prior_state.output));
            fs::remove_dir_all(path).ok();
        } else if path.is_file() {
            writer.line(&format!("rm {}", prior_state.output));
            fs::remove_file(path).ok();
        }
        Ok(())
    }

    fn refresh(&self, prior_state: &ExecState) -> Result<ApplyResult<ExecState>, BoxError> {
        let mut outputs = Map::new();
        outputs.insert("path".into(), Value::Str(prior_state.output.clone()));
        Ok(ApplyResult {
            outputs,
            state: Some(prior_state.clone()),
        })
    }
}

fn extract_string(inputs: &Map, key: &str) -> Result<String, BoxError> {
    inputs
        .get(key)
        .and_then(|v| v.as_str())
        .map(|s| s.to_owned())
        .ok_or_else(|| format!("missing or invalid '{key}' field").into())
}

fn extract_string_list(inputs: &Map, key: &str) -> Result<Vec<String>, BoxError> {
    let list = inputs
        .get(key)
        .and_then(|v| v.as_list())
        .ok_or_else(|| format!("missing or invalid '{key}' field"))?;
    list.iter()
        .map(|v| {
            v.as_str()
                .map(|s| s.to_owned())
                .ok_or_else(|| format!("'{key}' must contain only strings").into())
        })
        .collect()
}

fn hash_inputs(inputs: &Map) -> HashMap<String, String> {
    let globs = extract_string_list(inputs, "inputs").unwrap_or_default();
    let mut hashes = HashMap::new();
    for pattern in &globs {
        if let Ok(entries) = glob::glob(pattern) {
            for entry in entries.flatten() {
                if entry.is_file()
                    && let Ok(hash) = hash_file(&entry)
                {
                    hashes.insert(entry.to_string_lossy().into_owned(), hash);
                }
            }
        }
    }
    hashes
}

fn hash_file(path: &Path) -> Result<String, BoxError> {
    let contents = fs::read(path)?;
    let mut hasher = Sha256::new();
    hasher.update(&contents);
    Ok(format!("sha256:{:x}", hasher.finalize()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn resolve_expands_globs() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("test.txt");
        fs::write(&file, "hello").unwrap();

        let pattern = dir.path().join("*.txt").to_string_lossy().into_owned();
        let mut inputs = Map::new();
        inputs.insert("inputs".into(), Value::List(vec![Value::Str(pattern)]));

        let resource = ExecResource;
        let result = Resource::resolve(&resource, &inputs).unwrap();
        assert_eq!(result.inputs.len(), 1);
        assert_eq!(result.inputs[0].paths.len(), 1);
        assert!(result.inputs[0].paths[0].content_hash.starts_with("sha256:"));
    }

    #[test]
    fn plan_create_when_no_prior_state() {
        let mut inputs = Map::new();
        inputs.insert("command".into(), Value::Str("echo hi".into()));

        let resource = ExecResource;
        let result = Resource::plan(&resource, &inputs, None).unwrap();
        assert_eq!(result.action, PlanAction::Create);
    }

    #[test]
    fn plan_none_when_unchanged() {
        let mut inputs = Map::new();
        inputs.insert("command".into(), Value::Str("echo hi".into()));

        let prior = ExecState {
            command: "echo hi".into(),
            output: "out/".into(),
            input_hashes: HashMap::new(),
        };

        let resource = ExecResource;
        let result = Resource::plan(&resource, &inputs, Some(&prior)).unwrap();
        assert_eq!(result.action, PlanAction::None);
    }

    #[test]
    fn plan_update_when_command_changed() {
        let mut inputs = Map::new();
        inputs.insert("command".into(), Value::Str("echo bye".into()));

        let prior = ExecState {
            command: "echo hi".into(),
            output: "out/".into(),
            input_hashes: HashMap::new(),
        };

        let resource = ExecResource;
        let result = Resource::plan(&resource, &inputs, Some(&prior)).unwrap();
        assert_eq!(result.action, PlanAction::Update);
    }

    #[test]
    fn apply_runs_command() {
        let dir = tempfile::tempdir().unwrap();
        let output = dir.path().join("result.txt");

        let mut inputs = Map::new();
        inputs.insert(
            "command".into(),
            Value::Str(format!("echo hello > {}", output.display())),
        );
        inputs.insert("output".into(), Value::Str(output.to_string_lossy().into_owned()));
        inputs.insert("inputs".into(), Value::List(vec![]));

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
        let mut inputs = Map::new();
        inputs.insert("command".into(), Value::Str("false".into()));
        inputs.insert("output".into(), Value::Str("/dev/null".into()));
        inputs.insert("inputs".into(), Value::List(vec![]));

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
            output: output.to_string_lossy().into_owned(),
            input_hashes: HashMap::new(),
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
    fn hash_file_deterministic() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("test.txt");
        fs::write(&file, "hello world").unwrap();
        let h1 = hash_file(&file).unwrap();
        let h2 = hash_file(&file).unwrap();
        assert_eq!(h1, h2);
        assert!(h1.starts_with("sha256:"));
    }
}
