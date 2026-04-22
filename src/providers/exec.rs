use std::collections::BTreeMap;
use std::fs;
use std::io::BufReader;
use std::path::Path;
use std::process::{Command, Stdio};

use serde::{Deserialize, Serialize};

use crate::file_tracker::FileTracker;
use crate::output::BlockWriter;
use crate::provider::{
    ApplyResult, BoxError, DynResource, FuncSignature, PlanAction, PlanResult, Provider, Resource, ResourceKind,
};
use crate::sha256::SHA256;
use crate::value::Value;

/// Run a shell command, track inputs and outputs
#[derive(Debug, Deserialize, bit_derive::Schema)]
pub struct ExecInputs {
    /// Shell command to execute
    pub command: String,
    /// Output file or list of output files
    #[serde(default, deserialize_with = "string_or_vec_opt")]
    pub output: Vec<String>,
    /// Input file glob patterns
    #[serde(default)]
    pub inputs: Vec<String>,
    /// Working directory for the command
    #[serde(default)]
    pub dir: Option<String>,
    /// Shell command to run on `bit --clean` (replaces the default removal of outputs)
    #[serde(default)]
    pub clean: Option<String>,
    /// Shell command whose stdout is captured as state. Used to detect whether
    /// the resource exists and whether it has drifted.
    #[serde(default)]
    pub resolve: Option<String>,
    /// Shell command whose stdout is parsed as JSON and exposed as block outputs.
    #[serde(default)]
    pub outputs: Option<String>,
}

#[derive(Debug, Serialize, bit_derive::Schema)]
pub struct ExecOutputs {
    /// Output path (single-output blocks)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    /// Output paths (multi-output blocks)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub paths: Option<Vec<String>>,
    /// Dynamic key-value outputs from the `outputs` command.
    #[serde(flatten)]
    pub extra: crate::value::Map,
}

impl ExecOutputs {
    fn new(output: &[String], extra: crate::value::Map) -> Self {
        match output.len() {
            0 => Self {
                path: None,
                paths: None,
                extra,
            },
            1 => Self {
                path: Some(output[0].clone()),
                paths: None,
                extra,
            },
            _ => Self {
                path: None,
                paths: Some(output.to_vec()),
                extra,
            },
        }
    }
}

/// State persisted between runs for an exec block.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExecState {
    pub command: String,
    #[serde(default)]
    pub output: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dir: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub clean: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resolve: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resolve_output: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub outputs: Option<String>,
    #[serde(default, skip_serializing_if = "crate::value::Map::is_empty")]
    pub outputs_values: crate::value::Map,
}

/// Deserialize a field that can be either a single string or a list of strings.
/// Supports `#[serde(default)]` — when the field is absent serde uses `Default`
/// and this function is never called.
fn string_or_vec_opt<'de, D>(deserializer: D) -> Result<Vec<String>, D::Error>
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

/// Run a shell command under `sh -c`, piping stdout/stderr through the block
/// writer. Returns `Err` if the command exits non-zero.
fn run_command(command: &str, dir: Option<&str>, writer: &BlockWriter) -> Result<(), BoxError> {
    let mut cmd = Command::new("sh");
    cmd.arg("-c").arg(command).stdout(Stdio::piped()).stderr(Stdio::piped());
    if let Some(dir) = dir {
        cmd.current_dir(dir);
    }

    let mut child = cmd.spawn().map_err(|e| format!("failed to execute command: {e}"))?;

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
    Ok(())
}

/// Run a shell command silently and return its stdout. Used by `resolve` to
/// capture external resource state without streaming through the block writer.
fn run_capture(command: &str, dir: Option<&str>) -> Result<String, BoxError> {
    let mut cmd = Command::new("sh");
    cmd.arg("-c").arg(command).stdout(Stdio::piped()).stderr(Stdio::piped());
    if let Some(dir) = dir {
        cmd.current_dir(dir);
    }
    let output = cmd
        .output()
        .map_err(|e| format!("failed to execute resolve command: {e}"))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let detail = stderr.trim();
        return if detail.is_empty() {
            Err(format!("resolve command exited with {}", output.status).into())
        } else {
            Err(format!("resolve command exited with {}: {detail}", output.status).into())
        };
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// Run the outputs command and parse its stdout as a JSON object.
fn run_outputs(command: &str, dir: Option<&str>) -> Result<crate::value::Map, BoxError> {
    let stdout = run_capture(command, dir)?;
    let json: serde_json::Value =
        serde_json::from_str(&stdout).map_err(|e| format!("outputs command produced invalid JSON: {e}"))?;
    let map: crate::value::Map =
        serde_json::from_value(json).map_err(|e| format!("outputs command must produce a JSON object: {e}"))?;
    Ok(map)
}

pub struct ExecProvider;

impl Provider for ExecProvider {
    fn name(&self) -> &str {
        "exec"
    }

    fn resources(&self) -> Vec<Box<dyn DynResource>> {
        vec![Box::new(ExecResource), Box::new(ExecTestResource)]
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

    fn resolve(&self, inputs: &ExecInputs, tracker: &mut FileTracker) -> Result<BTreeMap<String, SHA256>, BoxError> {
        let mut files = BTreeMap::new();
        for pattern in &inputs.inputs {
            files.extend(tracker.hash_glob(pattern)?);
        }
        for output in &inputs.output {
            let path = Path::new(output);
            if path.is_file() {
                files.insert(output.clone(), tracker.hash_file(path)?);
            }
        }
        Ok(files)
    }

    fn plan(&self, inputs: &ExecInputs, prior_state: Option<&ExecState>) -> Result<PlanResult, BoxError> {
        let Some(prior) = prior_state else {
            return Ok(PlanResult {
                action: PlanAction::Create,
                description: inputs.command.clone(),
                reason: None,
            });
        };

        if prior.command != inputs.command || prior.dir != inputs.dir || prior.clean != inputs.clean {
            return Ok(PlanResult {
                action: PlanAction::Update,
                description: inputs.command.clone(),
                reason: None,
            });
        }

        if let Some(resolve) = &inputs.resolve {
            let current = run_capture(resolve, inputs.dir.as_deref());
            return match current {
                Err(_) => Ok(PlanResult {
                    action: PlanAction::Create,
                    description: inputs.command.clone(),
                    reason: Some("resolve: resource not found".into()),
                }),
                Ok(output) if prior.resolve_output.as_deref() != Some(output.as_str()) => Ok(PlanResult {
                    action: PlanAction::Update,
                    description: inputs.command.clone(),
                    reason: Some("resolve: resource state drifted".into()),
                }),
                Ok(_) => Ok(PlanResult {
                    action: PlanAction::None,
                    description: inputs.command.clone(),
                    reason: None,
                }),
            };
        }

        Ok(PlanResult {
            action: PlanAction::None,
            description: inputs.command.clone(),
            reason: None,
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

        run_command(&inputs.command, inputs.dir.as_deref(), writer)?;

        let resolve_output = inputs
            .resolve
            .as_deref()
            .map(|cmd| run_capture(cmd, inputs.dir.as_deref()))
            .transpose()?;

        let outputs_values = inputs
            .outputs
            .as_deref()
            .map(|cmd| run_outputs(cmd, inputs.dir.as_deref()))
            .transpose()?
            .unwrap_or_default();

        Ok(ApplyResult {
            outputs: ExecOutputs::new(&inputs.output, outputs_values.clone()),
            state: Some(ExecState {
                command: inputs.command.clone(),
                output: inputs.output.clone(),
                dir: inputs.dir.clone(),
                clean: inputs.clean.clone(),
                resolve: inputs.resolve.clone(),
                resolve_output,
                outputs: inputs.outputs.clone(),
                outputs_values,
            }),
        })
    }

    fn destroy(&self, prior_state: &ExecState, writer: &BlockWriter) -> Result<(), BoxError> {
        use crate::output::Event;
        if let Some(clean) = &prior_state.clean {
            writer.event(Event::Starting, clean);
            return run_command(clean, prior_state.dir.as_deref(), writer);
        }
        for output in &prior_state.output {
            let path = Path::new(output);
            if path.is_dir() {
                writer.event(Event::Starting, &format!("rm -rf {output}"));
                fs::remove_dir_all(path).ok();
            } else if path.is_file() {
                writer.event(Event::Starting, &format!("rm {output}"));
                fs::remove_file(path).ok();
            }
        }
        // When resolve is set without clean or outputs, destroy is a no-op
        // (the resource is external and bit doesn't know how to tear it down).
        Ok(())
    }
}

// --- exec.test resource ---

/// Run a command as a test (pass/fail by exit code)
#[derive(Debug, Deserialize, bit_derive::Schema)]
pub struct ExecTestInputs {
    /// Shell command to execute
    pub command: String,
    /// Input file glob patterns
    #[serde(default)]
    pub inputs: Vec<String>,
    /// Output files to track
    #[serde(default)]
    pub output: Vec<String>,
    /// Working directory for the command
    #[serde(default)]
    pub dir: Option<String>,
    /// Shell command to run on `bit --clean`
    #[serde(default)]
    pub clean: Option<String>,
}

#[derive(Debug, Serialize, bit_derive::Schema)]
pub struct ExecTestOutputs {
    /// Whether the test passed
    pub passed: bool,
}

/// State for an exec.test block.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExecTestState {
    pub command: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dir: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub clean: Option<String>,
}

struct ExecTestResource;

impl Resource for ExecTestResource {
    type State = ExecTestState;
    type Inputs = ExecTestInputs;
    type Outputs = ExecTestOutputs;

    fn name(&self) -> &str {
        "test"
    }

    fn kind(&self) -> ResourceKind {
        ResourceKind::Test
    }

    fn resolve(
        &self,
        inputs: &ExecTestInputs,
        tracker: &mut FileTracker,
    ) -> Result<BTreeMap<String, SHA256>, BoxError> {
        let mut files = BTreeMap::new();
        for pattern in &inputs.inputs {
            files.extend(tracker.hash_glob(pattern)?);
        }
        for output in &inputs.output {
            let path = Path::new(output);
            if path.is_file() {
                files.insert(output.clone(), tracker.hash_file(path)?);
            }
        }
        Ok(files)
    }

    fn plan(&self, inputs: &ExecTestInputs, prior_state: Option<&ExecTestState>) -> Result<PlanResult, BoxError> {
        let Some(prior) = prior_state else {
            return Ok(PlanResult {
                action: PlanAction::Create,
                description: inputs.command.clone(),
                reason: None,
            });
        };

        let action = if prior.command != inputs.command || prior.dir != inputs.dir || prior.clean != inputs.clean {
            PlanAction::Update
        } else {
            PlanAction::None
        };

        Ok(PlanResult {
            action,
            description: inputs.command.clone(),
            reason: None,
        })
    }

    fn apply(
        &self,
        inputs: &ExecTestInputs,
        _prior_state: Option<&ExecTestState>,
        writer: &BlockWriter,
    ) -> Result<ApplyResult<ExecTestState, ExecTestOutputs>, BoxError> {
        let passed = run_command(&inputs.command, inputs.dir.as_deref(), writer).is_ok();

        Ok(ApplyResult {
            outputs: ExecTestOutputs { passed },
            state: Some(ExecTestState {
                command: inputs.command.clone(),
                dir: inputs.dir.clone(),
                clean: inputs.clean.clone(),
            }),
        })
    }

    fn destroy(&self, prior_state: &ExecTestState, writer: &BlockWriter) -> Result<(), BoxError> {
        use crate::output::Event;
        if let Some(clean) = &prior_state.clean {
            writer.event(Event::Starting, clean);
            return run_command(clean, prior_state.dir.as_deref(), writer);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::value::Map;
    use std::fs;

    #[test]
    fn resolve_returns_globs_and_outputs() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("src");
        fs::create_dir_all(&src).unwrap();
        let file = src.join("main.rs");
        fs::write(&file, "fn main() {}").unwrap();
        let out_file = dir.path().join("out");
        fs::write(&out_file, "output").unwrap();

        let inputs = ExecInputs {
            command: "echo".into(),
            output: vec![out_file.to_string_lossy().into_owned()],
            inputs: vec![format!("{}/**/*.rs", dir.path().display())],
            dir: None,
            clean: None,
            resolve: None,
            outputs: None,
        };

        let resource = ExecResource;
        let mut tracker = crate::file_tracker::FileTracker::new();
        let result = Resource::resolve(&resource, &inputs, &mut tracker).unwrap();
        assert!(result.contains_key(&file.to_string_lossy().into_owned()));
        assert!(result.contains_key(&out_file.to_string_lossy().into_owned()));
    }

    #[test]
    fn plan_create_when_no_prior_state() {
        let inputs = ExecInputs {
            command: "echo hi".into(),
            output: vec!["out".into()],
            inputs: vec![],
            dir: None,
            clean: None,
            resolve: None,
            outputs: None,
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
            dir: None,
            clean: None,
            resolve: None,
            outputs: None,
        };
        let prior = ExecState {
            command: "echo hi".into(),
            output: vec!["out/".into()],
            dir: None,
            clean: None,
            resolve: None,
            resolve_output: None,
            outputs: None,
            outputs_values: crate::value::Map::new(),
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
            dir: None,
            clean: None,
            resolve: None,
            outputs: None,
        };
        let prior = ExecState {
            command: "echo hi".into(),
            output: vec!["out/".into()],
            dir: None,
            clean: None,
            resolve: None,
            resolve_output: None,
            outputs: None,
            outputs_values: crate::value::Map::new(),
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
            dir: None,
            clean: None,
            resolve: None,
            outputs: None,
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
            dir: None,
            clean: None,
            resolve: None,
            outputs: None,
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
            dir: None,
            clean: None,
            resolve: None,
            resolve_output: None,
            outputs: None,
            outputs_values: crate::value::Map::new(),
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
        assert_eq!(resources.len(), 2);
        assert_eq!(resources[0].name(), "exec");
        assert_eq!(resources[1].name(), "test");
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

    #[test]
    fn test_resource_passes_on_success() {
        let inputs = ExecTestInputs {
            command: "true".into(),
            inputs: vec![],
            output: vec![],
            dir: None,
            clean: None,
        };
        let resource = ExecTestResource;
        let out = crate::output::Output::new(&[]);
        let writer = out.writer("test");
        let result = Resource::apply(&resource, &inputs, None, &writer).unwrap();
        assert!(result.outputs.passed);
    }

    #[test]
    fn test_resource_fails_on_nonzero_exit() {
        let inputs = ExecTestInputs {
            command: "false".into(),
            inputs: vec![],
            output: vec![],
            dir: None,
            clean: None,
        };
        let resource = ExecTestResource;
        let out = crate::output::Output::new(&[]);
        let writer = out.writer("test");
        let result = Resource::apply(&resource, &inputs, None, &writer).unwrap();
        assert!(!result.outputs.passed);
    }

    #[test]
    fn test_resource_kind_is_test() {
        let resource = ExecTestResource;
        assert_eq!(Resource::kind(&resource), ResourceKind::Test);
    }

    #[test]
    fn plan_update_when_dir_changed() {
        let inputs = ExecInputs {
            command: "echo hi".into(),
            output: vec!["out".into()],
            inputs: vec![],
            dir: Some("/tmp".into()),
            clean: None,
            resolve: None,
            outputs: None,
        };
        let prior = ExecState {
            command: "echo hi".into(),
            output: vec!["out".into()],
            dir: None,
            clean: None,
            resolve: None,
            resolve_output: None,
            outputs: None,
            outputs_values: crate::value::Map::new(),
        };
        let resource = ExecResource;
        let result = Resource::plan(&resource, &inputs, Some(&prior)).unwrap();
        assert_eq!(result.action, PlanAction::Update);
    }

    #[test]
    fn apply_uses_working_dir() {
        let dir = tempfile::tempdir().unwrap();
        let canon = dir.path().canonicalize().unwrap();
        let output = canon.join("result.txt");
        let inputs = ExecInputs {
            command: format!("pwd > {}", output.display()),
            output: vec![output.to_string_lossy().into_owned()],
            inputs: vec![],
            dir: Some(canon.to_string_lossy().into_owned()),
            clean: None,
            resolve: None,
            outputs: None,
        };
        let resource = ExecResource;
        let out = crate::output::Output::new(&[]);
        let writer = out.writer("test");
        let result = Resource::apply(&resource, &inputs, None, &writer).unwrap();
        assert!(result.state.is_some());
        let state = result.state.unwrap();
        assert_eq!(state.dir.as_deref(), Some(canon.to_str().unwrap()));
        let content = fs::read_to_string(&output).unwrap();
        assert_eq!(content.trim(), canon.to_str().unwrap());
    }

    #[test]
    fn test_resource_uses_working_dir() {
        let dir = tempfile::tempdir().unwrap();
        let canon = dir.path().canonicalize().unwrap();
        let inputs = ExecTestInputs {
            command: format!("test \"$(pwd)\" = \"{}\"", canon.display()),
            inputs: vec![],
            output: vec![],
            dir: Some(canon.to_string_lossy().into_owned()),
            clean: None,
        };
        let resource = ExecTestResource;
        let out = crate::output::Output::new(&[]);
        let writer = out.writer("test");
        let result = Resource::apply(&resource, &inputs, None, &writer).unwrap();
        assert!(result.outputs.passed);
        assert_eq!(
            result.state.as_ref().unwrap().dir.as_deref(),
            Some(canon.to_str().unwrap())
        );
    }

    #[test]
    fn destroy_runs_clean_and_skips_output_removal() {
        let dir = tempfile::tempdir().unwrap();
        let output = dir.path().join("build_output");
        fs::create_dir_all(&output).unwrap();
        fs::write(output.join("file.txt"), "data").unwrap();
        let marker = dir.path().join("cleaned.txt");

        let state = ExecState {
            command: "echo hi".into(),
            output: vec![output.to_string_lossy().into_owned()],
            dir: None,
            clean: Some(format!("touch {}", marker.display())),
            resolve: None,
            resolve_output: None,
            outputs: None,
            outputs_values: crate::value::Map::new(),
        };

        let resource = ExecResource;
        let out = crate::output::Output::new(&[]);
        let writer = out.writer("test");
        Resource::destroy(&resource, &state, &writer).unwrap();

        assert!(marker.exists(), "clean command must have run");
        assert!(output.exists(), "outputs should NOT be removed when clean is set");
    }

    #[test]
    fn destroy_propagates_clean_failure() {
        let state = ExecState {
            command: "echo hi".into(),
            output: vec![],
            dir: None,
            clean: Some("false".into()),
            resolve: None,
            resolve_output: None,
            outputs: None,
            outputs_values: crate::value::Map::new(),
        };
        let resource = ExecResource;
        let out = crate::output::Output::new(&[]);
        let writer = out.writer("test");
        assert!(Resource::destroy(&resource, &state, &writer).is_err());
    }

    #[test]
    fn plan_update_when_clean_changed() {
        let inputs = ExecInputs {
            command: "echo hi".into(),
            output: vec!["out".into()],
            inputs: vec![],
            dir: None,
            clean: Some("rm -rf out".into()),
            resolve: None,
            outputs: None,
        };
        let prior = ExecState {
            command: "echo hi".into(),
            output: vec!["out".into()],
            dir: None,
            clean: None,
            resolve: None,
            resolve_output: None,
            outputs: None,
            outputs_values: crate::value::Map::new(),
        };
        let resource = ExecResource;
        let result = Resource::plan(&resource, &inputs, Some(&prior)).unwrap();
        assert_eq!(result.action, PlanAction::Update);
    }

    #[test]
    fn test_destroy_runs_clean() {
        let dir = tempfile::tempdir().unwrap();
        let marker = dir.path().join("cleaned.txt");
        let state = ExecTestState {
            command: "true".into(),
            dir: None,
            clean: Some(format!("touch {}", marker.display())),
        };
        let resource = ExecTestResource;
        let out = crate::output::Output::new(&[]);
        let writer = out.writer("test");
        Resource::destroy(&resource, &state, &writer).unwrap();
        assert!(marker.exists());
    }

    #[test]
    fn test_destroy_is_noop_without_clean() {
        let state = ExecTestState {
            command: "true".into(),
            dir: None,
            clean: None,
        };
        let resource = ExecTestResource;
        let out = crate::output::Output::new(&[]);
        let writer = out.writer("test");
        Resource::destroy(&resource, &state, &writer).unwrap();
    }

    #[test]
    fn test_plan_update_when_dir_changed() {
        let inputs = ExecTestInputs {
            command: "true".into(),
            inputs: vec![],
            output: vec![],
            dir: Some("/tmp".into()),
            clean: None,
        };
        let prior = ExecTestState {
            command: "true".into(),
            dir: None,
            clean: None,
        };
        let resource = ExecTestResource;
        let result = Resource::plan(&resource, &inputs, Some(&prior)).unwrap();
        assert_eq!(result.action, PlanAction::Update);
    }

    #[test]
    fn plan_create_when_resolve_fails() {
        let inputs = ExecInputs {
            command: "echo create".into(),
            output: vec![],
            inputs: vec![],
            dir: None,
            clean: None,
            resolve: Some("false".into()),
            outputs: None,
        };
        let prior = ExecState {
            command: "echo create".into(),
            output: vec![],
            dir: None,
            clean: None,
            resolve: Some("false".into()),
            resolve_output: Some("old-state".into()),
            outputs: None,
            outputs_values: crate::value::Map::new(),
        };
        let resource = ExecResource;
        let result = Resource::plan(&resource, &inputs, Some(&prior)).unwrap();
        assert_eq!(result.action, PlanAction::Create);
    }

    #[test]
    fn plan_update_when_resolve_output_drifted() {
        let inputs = ExecInputs {
            command: "echo create".into(),
            output: vec![],
            inputs: vec![],
            dir: None,
            clean: None,
            resolve: Some("echo new-state".into()),
            outputs: None,
        };
        let prior = ExecState {
            command: "echo create".into(),
            output: vec![],
            dir: None,
            clean: None,
            resolve: Some("echo new-state".into()),
            resolve_output: Some("old-state\n".into()),
            outputs: None,
            outputs_values: crate::value::Map::new(),
        };
        let resource = ExecResource;
        let result = Resource::plan(&resource, &inputs, Some(&prior)).unwrap();
        assert_eq!(result.action, PlanAction::Update);
    }

    #[test]
    fn plan_none_when_resolve_output_matches() {
        let inputs = ExecInputs {
            command: "echo create".into(),
            output: vec![],
            inputs: vec![],
            dir: None,
            clean: None,
            resolve: Some("echo current-state".into()),
            outputs: None,
        };
        let prior = ExecState {
            command: "echo create".into(),
            output: vec![],
            dir: None,
            clean: None,
            resolve: Some("echo current-state".into()),
            resolve_output: Some("current-state\n".into()),
            outputs: None,
            outputs_values: crate::value::Map::new(),
        };
        let resource = ExecResource;
        let result = Resource::plan(&resource, &inputs, Some(&prior)).unwrap();
        assert_eq!(result.action, PlanAction::None);
    }

    #[test]
    fn apply_captures_resolve_output() {
        let inputs = ExecInputs {
            command: "true".into(),
            output: vec![],
            inputs: vec![],
            dir: None,
            clean: None,
            resolve: Some("echo captured".into()),
            outputs: None,
        };
        let resource = ExecResource;
        let out = crate::output::Output::new(&[]);
        let writer = out.writer("test");
        let result = Resource::apply(&resource, &inputs, None, &writer).unwrap();
        let state = result.state.unwrap();
        assert_eq!(state.resolve_output.as_deref(), Some("captured\n"));
    }

    #[test]
    fn apply_no_resolve_output_without_resolve() {
        let dir = tempfile::tempdir().unwrap();
        let output = dir.path().join("result.txt");
        let inputs = ExecInputs {
            command: format!("echo hello > {}", output.display()),
            output: vec![output.to_string_lossy().into_owned()],
            inputs: vec![],
            dir: None,
            clean: None,
            resolve: None,
            outputs: None,
        };
        let resource = ExecResource;
        let out = crate::output::Output::new(&[]);
        let writer = out.writer("test");
        let result = Resource::apply(&resource, &inputs, None, &writer).unwrap();
        let state = result.state.unwrap();
        assert!(state.resolve_output.is_none());
    }

    #[test]
    fn output_empty_when_no_outputs() {
        let outputs = ExecOutputs::new(&[], crate::value::Map::new());
        assert!(outputs.path.is_none());
        assert!(outputs.paths.is_none());
    }

    #[test]
    fn apply_captures_json_outputs() {
        let inputs = ExecInputs {
            command: "true".into(),
            output: vec![],
            inputs: vec![],
            dir: None,
            clean: None,
            resolve: None,
            outputs: Some("echo '{\"name\":\"test\",\"version\":\"1.0\"}'".into()),
        };
        let resource = ExecResource;
        let out = crate::output::Output::new(&[]);
        let writer = out.writer("test");
        let result = Resource::apply(&resource, &inputs, None, &writer).unwrap();
        let state = result.state.unwrap();
        assert_eq!(state.outputs_values.get("name").and_then(|v| v.as_str()), Some("test"));
        assert_eq!(result.outputs.extra.get("name").and_then(|v| v.as_str()), Some("test"));
        assert_eq!(
            result.outputs.extra.get("version").and_then(|v| v.as_str()),
            Some("1.0")
        );
    }

    #[test]
    fn run_capture_includes_stderr_on_failure() {
        let err = run_capture("echo 'something went wrong' >&2; exit 1", None).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("something went wrong"),
            "expected stderr in error, got: {msg}"
        );
    }

    #[test]
    fn run_capture_omits_stderr_when_empty() {
        let err = run_capture("exit 1", None).unwrap_err();
        let msg = err.to_string();
        assert_eq!(msg, "resolve command exited with exit status: 1");
    }
}
