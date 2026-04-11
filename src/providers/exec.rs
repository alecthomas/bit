use std::fs;
use std::io::{BufRead, BufReader};
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
///
/// Single-output blocks get `path`; multi-output blocks get `paths`.
#[derive(Debug, Serialize)]
pub struct ExecOutputs {
    /// First output path (present for single-output blocks).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    /// All output paths (present only for multi-output blocks).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub paths: Option<Vec<String>>,
}

impl ExecOutputs {
    fn from_paths(output: &[String]) -> Self {
        if output.len() == 1 {
            Self {
                path: Some(output[0].clone()),
                paths: None,
            }
        } else {
            Self {
                path: None,
                paths: Some(output.to_vec()),
            }
        }
    }
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

    fn resolve(&self, inputs: &ExecInputs) -> Result<Vec<crate::provider::ResolvedFile>, BoxError> {
        use crate::provider::ResolvedFile;
        let mut files: Vec<ResolvedFile> = inputs
            .inputs
            .iter()
            .map(|p| ResolvedFile::InputGlob(p.clone()))
            .collect();
        for output in &inputs.output {
            files.push(ResolvedFile::Output(Path::new(output).to_path_buf()));
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

        let action = if prior.command != inputs.command {
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
            outputs: ExecOutputs::from_paths(&inputs.output),
            state: Some(ExecState {
                command: inputs.command.clone(),
                output: inputs.output.clone(),
            }),
        })
    }

    fn destroy(&self, prior_state: &ExecState, writer: &BlockWriter) -> Result<(), BoxError> {
        use crate::output::Event;
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
        Ok(())
    }

    fn refresh(&self, prior_state: &ExecState) -> Result<ApplyResult<ExecState, ExecOutputs>, BoxError> {
        Ok(ApplyResult {
            outputs: ExecOutputs::from_paths(&prior_state.output),
            state: Some(prior_state.clone()),
        })
    }
}

/// Parse stdout into a CTRF report, optionally applying a jq transform first.
/// Falls back to a synthesized report from exit code if stdout isn't CTRF.
fn parse_ctrf_output(
    stdout: &str,
    transform: &Option<String>,
    command: &str,
    passed: bool,
) -> Result<crate::ctrf::Report, BoxError> {
    use crate::ctrf::{self, Report, Results, Summary, Test, Tool};

    if let Some(expr) = transform {
        let ctrf_json = crate::jq::transform(expr, stdout)?;
        return Report::from_json(&ctrf_json).map_err(|e| format!("failed to parse CTRF JSON: {e}").into());
    }

    if let Ok(report) = Report::from_json(stdout.trim()) {
        return Ok(report);
    }

    // Synthesize from exit code
    let status = if passed {
        ctrf::Status::Passed
    } else {
        ctrf::Status::Failed
    };
    let tests = vec![Test {
        name: command.to_owned(),
        status,
        duration: 0,
        suite: None,
        message: None,
        trace: None,
        file_path: None,
        flaky: None,
    }];
    Ok(Report {
        report_format: "CTRF".into(),
        spec_version: "0.0.1".into(),
        results: Results {
            tool: Tool {
                name: command.to_owned(),
                version: None,
            },
            summary: Summary::from_tests(&tests),
            tests,
            environment: None,
        },
    })
}

// --- exec.test resource ---

/// Typed inputs for an exec.test block.
#[derive(Debug, Deserialize)]
pub struct ExecTestInputs {
    pub command: String,
    #[serde(default)]
    pub inputs: Vec<String>,
    #[serde(default)]
    pub output: Vec<String>,
    /// Optional jq expression to transform stdout into CTRF JSON.
    /// Only used when `format = "ctrf"`.
    #[serde(default)]
    pub transform: Option<String>,
    /// Output format. If "ctrf", stdout is parsed as CTRF JSON (or
    /// transformed via `transform` first). If omitted, stdout/stderr
    /// are displayed normally and pass/fail is determined by exit code.
    #[serde(default)]
    pub format: Option<String>,
}

/// Outputs from an exec.test block.
#[derive(Debug, Serialize)]
pub struct ExecTestOutputs {
    pub passed: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub report: Option<crate::ctrf::Report>,
}

/// State for an exec.test block.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExecTestState {
    pub command: String,
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

    fn resolve(&self, inputs: &ExecTestInputs) -> Result<Vec<crate::provider::ResolvedFile>, BoxError> {
        use crate::provider::ResolvedFile;
        let mut files: Vec<ResolvedFile> = inputs
            .inputs
            .iter()
            .map(|p| ResolvedFile::InputGlob(p.clone()))
            .collect();
        for output in &inputs.output {
            files.push(ResolvedFile::Output(Path::new(output).to_path_buf()));
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

        let action = if prior.command != inputs.command {
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
        let use_ctrf = inputs.format.as_deref() == Some("ctrf");

        let mut child = Command::new("sh")
            .arg("-c")
            .arg(&inputs.command)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| format!("failed to execute command: {e}"))?;

        let stdout_handle = child.stdout.take();
        let stderr_handle = child.stderr.take();

        // In CTRF mode, capture stdout for parsing. Otherwise pipe both to user.
        let mut stdout_buf = String::new();
        std::thread::scope(|s| {
            if use_ctrf {
                let stdout_thread = stdout_handle.map(|out| {
                    s.spawn(|| {
                        let mut buf = String::new();
                        let mut reader = BufReader::new(out);
                        loop {
                            let mut line = String::new();
                            match reader.read_line(&mut line) {
                                Ok(0) | Err(_) => break,
                                Ok(_) => buf.push_str(&line),
                            }
                        }
                        buf
                    })
                });
                if let Some(err) = stderr_handle {
                    s.spawn(|| writer.pipe_stderr(BufReader::new(err)));
                }
                if let Some(handle) = stdout_thread {
                    stdout_buf = handle.join().unwrap_or_default();
                }
            } else {
                if let Some(out) = stdout_handle {
                    s.spawn(|| writer.pipe_stdout(BufReader::new(out)));
                }
                if let Some(err) = stderr_handle {
                    s.spawn(|| writer.pipe_stderr(BufReader::new(err)));
                }
            }
        });

        let status = child.wait().map_err(|e| format!("failed to wait for command: {e}"))?;
        let passed = status.success();

        let report = if use_ctrf {
            Some(parse_ctrf_output(
                &stdout_buf,
                &inputs.transform,
                &inputs.command,
                passed,
            )?)
        } else {
            None
        };

        Ok(ApplyResult {
            outputs: ExecTestOutputs { passed, report },
            state: Some(ExecTestState {
                command: inputs.command.clone(),
            }),
        })
    }

    fn destroy(&self, _prior_state: &ExecTestState, _writer: &BlockWriter) -> Result<(), BoxError> {
        Ok(())
    }

    fn refresh(&self, prior_state: &ExecTestState) -> Result<ApplyResult<ExecTestState, ExecTestOutputs>, BoxError> {
        Ok(ApplyResult {
            outputs: ExecTestOutputs {
                passed: true,
                report: None,
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
    fn resolve_returns_globs_and_outputs() {
        use crate::provider::ResolvedFile;

        let inputs = ExecInputs {
            command: "echo".into(),
            output: vec!["out".into()],
            inputs: vec!["src/**/*.rs".into()],
        };

        let resource = ExecResource;
        let result = Resource::resolve(&resource, &inputs).unwrap();
        assert_eq!(result.len(), 2);
        assert_eq!(result[0], ResolvedFile::InputGlob("src/**/*.rs".into()));
        assert_eq!(result[1], ResolvedFile::Output(PathBuf::from("out")));
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
            transform: None,
            format: None,
        };
        let resource = ExecTestResource;
        let out = crate::output::Output::new(&[]);
        let writer = out.writer("test");
        let result = Resource::apply(&resource, &inputs, None, &writer).unwrap();
        assert!(result.outputs.passed);
        assert!(result.outputs.report.is_none());
    }

    #[test]
    fn test_resource_fails_on_nonzero_exit() {
        let inputs = ExecTestInputs {
            command: "false".into(),
            inputs: vec![],
            output: vec![],
            transform: None,
            format: None,
        };
        let resource = ExecTestResource;
        let out = crate::output::Output::new(&[]);
        let writer = out.writer("test");
        let result = Resource::apply(&resource, &inputs, None, &writer).unwrap();
        assert!(!result.outputs.passed);
    }

    #[test]
    fn test_resource_parses_ctrf_stdout() {
        let ctrf = r#"{"reportFormat":"CTRF","specVersion":"0.0.1","results":{"tool":{"name":"test"},"summary":{"tests":1,"passed":1,"failed":0,"skipped":0},"tests":[{"name":"it_works","status":"passed","duration":5}]}}"#;
        let inputs = ExecTestInputs {
            command: format!("echo '{ctrf}'"),
            inputs: vec![],
            output: vec![],
            transform: None,
            format: Some("ctrf".into()),
        };
        let resource = ExecTestResource;
        let out = crate::output::Output::new(&[]);
        let writer = out.writer("test");
        let result = Resource::apply(&resource, &inputs, None, &writer).unwrap();
        assert!(result.outputs.passed);
        let report = result.outputs.report.unwrap();
        assert_eq!(report.results.summary.tests, 1);
        assert_eq!(report.results.tests[0].name, "it_works");
    }

    #[test]
    fn test_resource_applies_jq_transform() {
        let inputs = ExecTestInputs {
            command: r#"echo '{"ok": true, "count": 2}'"#.into(),
            inputs: vec![],
            output: vec![],
            format: Some("ctrf".into()),
            transform: Some(
                r#"
                {
                    reportFormat: "CTRF",
                    specVersion: "0.0.1",
                    results: {
                        tool: { name: "custom" },
                        summary: { tests: .count, passed: .count, failed: 0, skipped: 0 },
                        tests: [{ name: "all", status: "passed", duration: 0 }]
                    }
                }
            "#
                .into(),
            ),
        };
        let resource = ExecTestResource;
        let out = crate::output::Output::new(&[]);
        let writer = out.writer("test");
        let result = Resource::apply(&resource, &inputs, None, &writer).unwrap();
        assert!(result.outputs.passed);
        let report = result.outputs.report.unwrap();
        assert_eq!(report.results.tool.name, "custom");
        assert_eq!(report.results.summary.tests, 2);
    }

    #[test]
    fn test_resource_kind_is_test() {
        let resource = ExecTestResource;
        assert_eq!(Resource::kind(&resource), ResourceKind::Test);
    }
}
