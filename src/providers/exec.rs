mod results;

use std::collections::BTreeMap;
use std::fs;
use std::io::{BufRead, BufReader};
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};

use serde::{Deserialize, Serialize};

use super::ReportMode;
use crate::file_tracker::FileTracker;
use crate::output::{BlockWriter, Message};
use crate::provider::{
    Ambient, ApplyResult, BoxError, CachePolicy, DynResource, FuncSignature, PlanAction, PlanResult, Provider,
    Resource, ResourceKind,
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
    #[schema(typ = crate::schema::optional_string_or_list())]
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

/// Build a `bash -eo pipefail -c` command with piped stdout and stderr. `-e`
/// makes a multi-line script fail at its first failing line; without it, only
/// the last line's exit status counts, so a trailing `echo` would hide
/// failures. `pipefail` does the same for earlier commands in a pipeline,
/// which is why this uses bash rather than POSIX `sh`.
fn shell(command: &str, dir: Option<&str>) -> Command {
    let mut cmd = Command::new("bash");
    cmd.args(["-eo", "pipefail", "-c"])
        .arg(command)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if let Some(dir) = dir {
        cmd.current_dir(dir);
    }
    cmd
}

/// Run a shell command under `bash -eo pipefail`, piping stdout/stderr through the block
/// writer. Returns `Err` if the command exits non-zero.
fn run_command(command: &str, dir: Option<&str>, writer: &BlockWriter) -> Result<(), BoxError> {
    let mut child = shell(command, dir)
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
    Ok(())
}

/// Run a shell command silently and return its stdout. Used by `resolve` to
/// capture external resource state without streaming through the block writer.
fn run_capture(command: &str, dir: Option<&str>) -> Result<String, BoxError> {
    let output = shell(command, dir)
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

/// Provider for running arbitrary shell commands.
pub struct ExecProvider {
    tracker: Arc<Mutex<FileTracker>>,
}

impl ExecProvider {
    /// Create a new exec provider with a shared file tracker.
    pub fn new(tracker: Arc<Mutex<FileTracker>>) -> Self {
        Self { tracker }
    }
}

impl Provider for ExecProvider {
    fn name(&self) -> &str {
        "exec"
    }

    fn resources(&self) -> Vec<Box<dyn DynResource>> {
        vec![
            Box::new(ExecResource {
                tracker: self.tracker.clone(),
            }),
            Box::new(ExecTestResource {
                tracker: self.tracker.clone(),
            }),
        ]
    }

    fn functions(&self) -> Vec<FuncSignature> {
        vec![]
    }

    fn call_function(&self, name: &str, _args: &[Value]) -> Result<Value, BoxError> {
        Err(format!("exec provider has no function '{name}'").into())
    }
}

struct ExecResource {
    tracker: Arc<Mutex<FileTracker>>,
}

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

    /// The shell that runs the command. The programs the command itself
    /// runs are unknown, so `inputs` must name any that matter.
    fn ambient(&self, _inputs: &ExecInputs) -> Vec<Ambient> {
        vec![crate::providers::version("bash", &["--version"], None)]
    }

    fn resolve(&self, inputs: &ExecInputs) -> Result<BTreeMap<String, SHA256>, BoxError> {
        let mut tracker = self.tracker.lock().expect("tracker lock");
        let mut files = BTreeMap::new();
        for pattern in &inputs.inputs {
            files.extend(tracker.hash_glob(pattern)?);
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
            super::remove_path(Path::new(output), writer)?;
        }
        // When resolve is set without clean or outputs, destroy is a no-op
        // (the resource is external and bit doesn't know how to tear it down).
        Ok(())
    }

    /// `resolve` and `outputs` make a block a window onto something outside
    /// the worktree: `resolve` reports whether an external resource exists,
    /// and `outputs` reads values back out of it. Neither is something a
    /// receipt written elsewhere can vouch for, so those blocks stay local
    /// and the rest share the files they produce.
    fn cache_policy(&self, inputs: &ExecInputs) -> CachePolicy {
        if inputs.resolve.is_some() || inputs.outputs.is_some() {
            CachePolicy::Local
        } else {
            CachePolicy::Shared { version: 1 }
        }
    }

    fn output_keys(&self, inputs: &ExecInputs) -> Vec<String> {
        inputs
            .output
            .iter()
            .map(|o| o.trim_end_matches('/').to_owned())
            .collect()
    }
}

// --- exec.test resource ---

/// Run a command as a test (pass/fail by exit code and any failures reported via `results`)
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
    /// jq filter mapping the command's JSON stdout to test results. Each JSON
    /// document or JSONL line is one filter input; other stdout lines are
    /// logs. Each filter output is `{suite?, name?, event?, status?,
    /// duration?, output?}`, about a test if it has a `name` and otherwise
    /// about its suite. `event` is `start` or `end`, and `status` (`pass`,
    /// `fail`, or `skip`) also ends the test or suite. `duration` is in
    /// seconds; without it, the time since `start` is used. A suite is
    /// summarized when it ends, or when the command exits.
    #[serde(default)]
    pub results: Option<String>,
    /// What to show from `results`: a line per suite (`suites`), a line per
    /// test (`tests`), or a line per test plus logs as they arrive
    /// (`verbose`). Outside `verbose`, logs are shown only if the test fails.
    #[serde(default)]
    pub report: ReportMode,
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub results: Option<String>,
}

/// Run a test command whose stdout is mapped through `filter`. Stderr streams
/// in verbose mode; otherwise it is held with the other logs and shown only
/// if the run fails. Returns whether the command exited zero and every mapped
/// test and suite passed.
fn run_with_results(
    filter: &results::ResultsFilter,
    inputs: &ExecTestInputs,
    writer: &BlockWriter,
) -> Result<bool, BoxError> {
    let mut child = shell(&inputs.command, inputs.dir.as_deref())
        .spawn()
        .map_err(|e| format!("failed to execute command: {e}"))?;

    let stdout = child.stdout.take();
    let stderr = child.stderr.take();

    let verbose = inputs.report == ReportMode::Verbose;
    let mut report = results::Report::new(filter, writer, inputs.report);
    let (mapped, stderr_lines) = std::thread::scope(|s| {
        let stderr_handle = stderr.map(|err| {
            s.spawn(move || {
                if verbose {
                    writer.pipe_stderr(BufReader::new(err));
                    Vec::new()
                } else {
                    BufReader::new(err).lines().map_while(Result::ok).collect()
                }
            })
        });
        let mut mapped = Ok(());
        if let Some(out) = stdout {
            // Keep draining stdout after a mapping error so the command
            // can't block on a full pipe.
            for line in BufReader::new(out).lines().map_while(Result::ok) {
                if let Some(message) = Message::parse(&line) {
                    writer.message(&message);
                } else if mapped.is_ok() {
                    mapped = report.line(&line);
                }
            }
        }
        let stderr_lines: Vec<String> = stderr_handle
            .map(|handle| handle.join().unwrap_or_default())
            .unwrap_or_default();
        (mapped, stderr_lines)
    });

    let status = child.wait().map_err(|e| format!("failed to wait for command: {e}"))?;
    mapped?;
    let mut outcome = report.finish();
    let passed = status.success() && outcome.passed;
    if !passed {
        outcome.logs.extend(stderr_lines);
        writer.stderr_details(&outcome.logs);
    }
    Ok(passed)
}

struct ExecTestResource {
    tracker: Arc<Mutex<FileTracker>>,
}

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

    /// The shell that runs the command. The programs the command itself
    /// runs are unknown, so `inputs` must name any that matter.
    fn ambient(&self, _inputs: &ExecTestInputs) -> Vec<Ambient> {
        vec![crate::providers::version("bash", &["--version"], None)]
    }

    fn resolve(&self, inputs: &ExecTestInputs) -> Result<BTreeMap<String, SHA256>, BoxError> {
        let mut tracker = self.tracker.lock().expect("tracker lock");
        let mut files = BTreeMap::new();
        for pattern in &inputs.inputs {
            files.extend(tracker.hash_glob(pattern)?);
        }
        Ok(files)
    }

    fn plan(&self, inputs: &ExecTestInputs, prior_state: Option<&ExecTestState>) -> Result<PlanResult, BoxError> {
        // Surface filter syntax errors at plan time rather than after the tests run.
        if let Some(code) = &inputs.results {
            results::ResultsFilter::compile(code)?;
        }

        let Some(prior) = prior_state else {
            return Ok(PlanResult {
                action: PlanAction::Create,
                description: inputs.command.clone(),
                reason: None,
            });
        };

        let action = if prior.command != inputs.command
            || prior.dir != inputs.dir
            || prior.clean != inputs.clean
            || prior.results != inputs.results
        {
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
        let passed = match &inputs.results {
            Some(code) => run_with_results(&results::ResultsFilter::compile(code)?, inputs, writer)?,
            None => run_command(&inputs.command, inputs.dir.as_deref(), writer).is_ok(),
        };

        Ok(ApplyResult {
            outputs: ExecTestOutputs { passed },
            state: Some(ExecTestState {
                command: inputs.command.clone(),
                dir: inputs.dir.clone(),
                clean: inputs.clean.clone(),
                results: inputs.results.clone(),
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

    fn cache_policy(&self, _inputs: &ExecTestInputs) -> CachePolicy {
        CachePolicy::Shared { version: 1 }
    }

    fn output_keys(&self, inputs: &ExecTestInputs) -> Vec<String> {
        inputs
            .output
            .iter()
            .map(|o| o.trim_end_matches('/').to_owned())
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::value::Map;
    use std::fs;

    fn test_tracker() -> Arc<Mutex<FileTracker>> {
        Arc::new(Mutex::new(FileTracker::new()))
    }

    #[test]
    fn resolve_returns_only_input_globs() {
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

        let tracker = test_tracker();
        let resource = ExecResource {
            tracker: tracker.clone(),
        };
        let result = Resource::resolve(&resource, &inputs).unwrap();
        assert!(result.contains_key(&file.to_string_lossy().into_owned()));
        assert!(!result.contains_key(&out_file.to_string_lossy().into_owned()));
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
        let resource = ExecResource {
            tracker: test_tracker(),
        };
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
        let resource = ExecResource {
            tracker: test_tracker(),
        };
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
        let resource = ExecResource {
            tracker: test_tracker(),
        };
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
        let resource = ExecResource {
            tracker: test_tracker(),
        };
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
        let resource = ExecResource {
            tracker: test_tracker(),
        };
        let out = crate::output::Output::new(&[]);
        let writer = out.writer("test");
        assert!(Resource::apply(&resource, &inputs, None, &writer).is_err());
    }

    #[test]
    fn apply_fails_when_an_earlier_line_fails() {
        let inputs = ExecInputs {
            command: "false\necho \"::success::done\"".into(),
            ..exec_inputs(vec![])
        };
        let resource = ExecResource {
            tracker: test_tracker(),
        };
        let out = crate::output::Output::silent();
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

        let resource = ExecResource {
            tracker: test_tracker(),
        };
        let out = crate::output::Output::new(&[]);
        let writer = out.writer("test");
        Resource::destroy(&resource, &state, &writer).unwrap();
        assert!(!output.exists());
    }

    #[test]
    fn provider_registration() {
        let provider = ExecProvider::new(test_tracker());
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

        let resource: Box<dyn DynResource> = Box::new(ExecResource {
            tracker: test_tracker(),
        });
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
        let inputs = test_inputs("true", None);
        let resource = ExecTestResource {
            tracker: test_tracker(),
        };
        let out = crate::output::Output::new(&[]);
        let writer = out.writer("test");
        let result = Resource::apply(&resource, &inputs, None, &writer).unwrap();
        assert!(result.outputs.passed);
    }

    #[test]
    fn test_resource_fails_on_nonzero_exit() {
        let inputs = test_inputs("false", None);
        let resource = ExecTestResource {
            tracker: test_tracker(),
        };
        let out = crate::output::Output::new(&[]);
        let writer = out.writer("test");
        let result = Resource::apply(&resource, &inputs, None, &writer).unwrap();
        assert!(!result.outputs.passed);
    }

    #[test]
    fn test_resource_fails_when_an_earlier_line_fails() {
        assert!(!apply_test(&test_inputs("false\ntrue", None)).unwrap());
    }

    #[test]
    fn test_resource_fails_when_an_earlier_pipeline_command_fails() {
        assert!(!apply_test(&test_inputs("false | true", None)).unwrap());
    }

    #[test]
    fn test_results_fail_when_an_earlier_line_fails() {
        let command = "false\necho '{\"n\":\"a\",\"ok\":true}'";
        assert!(!apply_test(&test_inputs(command, Some(JSONL_RESULTS))).unwrap());
    }

    fn test_inputs(command: &str, results: Option<&str>) -> ExecTestInputs {
        ExecTestInputs {
            command: command.into(),
            inputs: vec![],
            output: vec![],
            dir: None,
            clean: None,
            results: results.map(Into::into),
            report: ReportMode::default(),
        }
    }

    fn apply_test(inputs: &ExecTestInputs) -> Result<bool, BoxError> {
        let resource = ExecTestResource {
            tracker: test_tracker(),
        };
        let out = crate::output::Output::silent();
        let writer = out.writer("test");
        Ok(Resource::apply(&resource, inputs, None, &writer)?.outputs.passed)
    }

    #[test]
    fn test_report_schema_lists_modes() {
        use crate::schema::Schema;
        let schema = ExecTestInputs::schema();
        let (_, report) = schema.fields.iter().find(|(name, _)| name == "report").unwrap();
        assert_eq!(report.typ.to_string(), r#"("suites" | "tests" | "verbose")?"#);
    }

    const JSONL_RESULTS: &str = r#"{name: .n, status: (if .ok then "pass" else "fail" end)}"#;

    #[test]
    fn test_results_pass_when_all_mapped_tests_pass() {
        let command = r#"echo '{"n":"a","ok":true}'; echo '{"n":"b","ok":true}'"#;
        assert!(apply_test(&test_inputs(command, Some(JSONL_RESULTS))).unwrap());
    }

    #[test]
    fn test_results_fail_when_a_mapped_test_fails_despite_zero_exit() {
        let command = r#"echo '{"n":"a","ok":true}'; echo '{"n":"b","ok":false}'"#;
        assert!(!apply_test(&test_inputs(command, Some(JSONL_RESULTS))).unwrap());
    }

    #[test]
    fn test_results_fail_on_nonzero_exit_despite_passing_tests() {
        let command = r#"echo '{"n":"a","ok":true}'; exit 1"#;
        assert!(!apply_test(&test_inputs(command, Some(JSONL_RESULTS))).unwrap());
    }

    #[test]
    fn test_results_map_a_single_document() {
        let command = r#"printf '[\n  {"n":"a","ok":true},\n  {"n":"b","ok":false}\n]\n'"#;
        let results = format!(".[] | {JSONL_RESULTS}");
        assert!(!apply_test(&test_inputs(command, Some(&results))).unwrap());
    }

    #[test]
    fn test_results_filter_error_is_an_apply_error() {
        let err = apply_test(&test_inputs("echo '{}'", Some(r#"error("boom")"#)))
            .err()
            .unwrap();
        assert!(err.to_string().starts_with("results filter failed:"), "{err}");
    }

    #[test]
    fn test_plan_rejects_invalid_results_filter() {
        let resource = ExecTestResource {
            tracker: test_tracker(),
        };
        let err = Resource::plan(&resource, &test_inputs("true", Some(".[")), None)
            .err()
            .unwrap();
        assert!(err.to_string().starts_with("invalid results filter:"), "{err}");
    }

    #[test]
    fn test_plan_update_when_results_changed() {
        let prior = ExecTestState {
            command: "true".into(),
            dir: None,
            clean: None,
            results: None,
        };
        let resource = ExecTestResource {
            tracker: test_tracker(),
        };
        let result = Resource::plan(&resource, &test_inputs("true", Some(".")), Some(&prior)).unwrap();
        assert_eq!(result.action, PlanAction::Update);
    }

    fn exec_inputs(output: Vec<String>) -> ExecInputs {
        ExecInputs {
            command: "make".into(),
            output,
            inputs: vec![],
            dir: None,
            clean: None,
            resolve: None,
            outputs: None,
        }
    }

    #[test]
    fn declared_outputs_are_cache_keys_without_their_trailing_slash() {
        let resource = ExecResource {
            tracker: test_tracker(),
        };
        let inputs = exec_inputs(vec!["dist/".into(), "bin/app".into()]);

        assert_eq!(Resource::output_keys(&resource, &inputs), ["dist", "bin/app"]);
    }

    #[test]
    fn blocks_producing_files_are_shared() {
        let resource = ExecResource {
            tracker: test_tracker(),
        };
        assert_eq!(
            Resource::cache_policy(&resource, &exec_inputs(vec!["dist/".into()])),
            CachePolicy::Shared { version: 1 }
        );
    }

    /// `resolve` and `outputs` read state that lives outside the worktree, so
    /// no other worktree can act on what this one observed.
    #[test]
    fn blocks_watching_external_state_stay_local() {
        let resource = ExecResource {
            tracker: test_tracker(),
        };

        let mut inputs = exec_inputs(vec![]);
        inputs.resolve = Some("docker inspect thing".into());
        assert_eq!(Resource::cache_policy(&resource, &inputs), CachePolicy::Local);

        let mut inputs = exec_inputs(vec![]);
        inputs.outputs = Some("echo '{}'".into());
        assert_eq!(Resource::cache_policy(&resource, &inputs), CachePolicy::Local);
    }

    #[test]
    fn test_resource_kind_is_test() {
        let resource = ExecTestResource {
            tracker: test_tracker(),
        };
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
        let resource = ExecResource {
            tracker: test_tracker(),
        };
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
        let resource = ExecResource {
            tracker: test_tracker(),
        };
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
            dir: Some(canon.to_string_lossy().into_owned()),
            ..test_inputs(&format!("test \"$(pwd)\" = \"{}\"", canon.display()), None)
        };
        let resource = ExecTestResource {
            tracker: test_tracker(),
        };
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

        let resource = ExecResource {
            tracker: test_tracker(),
        };
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
        let resource = ExecResource {
            tracker: test_tracker(),
        };
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
        let resource = ExecResource {
            tracker: test_tracker(),
        };
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
            results: None,
        };
        let resource = ExecTestResource {
            tracker: test_tracker(),
        };
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
            results: None,
        };
        let resource = ExecTestResource {
            tracker: test_tracker(),
        };
        let out = crate::output::Output::new(&[]);
        let writer = out.writer("test");
        Resource::destroy(&resource, &state, &writer).unwrap();
    }

    #[test]
    fn test_plan_update_when_dir_changed() {
        let inputs = ExecTestInputs {
            dir: Some("/tmp".into()),
            ..test_inputs("true", None)
        };
        let prior = ExecTestState {
            command: "true".into(),
            dir: None,
            clean: None,
            results: None,
        };
        let resource = ExecTestResource {
            tracker: test_tracker(),
        };
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
        let resource = ExecResource {
            tracker: test_tracker(),
        };
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
        let resource = ExecResource {
            tracker: test_tracker(),
        };
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
        let resource = ExecResource {
            tracker: test_tracker(),
        };
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
        let resource = ExecResource {
            tracker: test_tracker(),
        };
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
        let resource = ExecResource {
            tracker: test_tracker(),
        };
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
        let resource = ExecResource {
            tracker: test_tracker(),
        };
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
