use std::collections::HashMap;
use std::io::{BufRead, BufReader};
use std::process::{Command, Stdio};
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::output::BlockWriter;
use crate::provider::{ApplyResult, BoxError, PlanAction, PlanResult, ResolvedFile, Resource, ResourceKind};

use super::GoEnv;

/// Inputs for a `go.test` block.
#[derive(Debug, Deserialize)]
pub struct GoTestInputs {
    /// Go package to test (e.g. "./...").
    pub package: String,
    /// Extra flags passed to `go test`.
    #[serde(default)]
    pub flags: Vec<String>,
    /// Show individual test results instead of package summaries.
    #[serde(default)]
    pub verbose: bool,
    #[serde(flatten)]
    pub env: GoEnv,
}

/// Outputs from a `go.test` block.
#[derive(Debug, Serialize)]
pub struct GoTestOutputs {
    pub passed: bool,
}

/// Persisted state for a `go.test` block.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GoTestState {
    pub package: String,
    pub flags: Vec<String>,
    #[serde(flatten)]
    pub env: GoEnv,
}

/// A single `go test -json` event line.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "PascalCase")]
struct GoTestEvent {
    action: String,
    #[serde(default)]
    package: String,
    #[serde(default)]
    test: Option<String>,
    #[serde(default)]
    elapsed: Option<f64>,
    #[serde(default)]
    output: Option<String>,
}

struct TestJsonResult {
    passed: bool,
    had_events: bool,
}

/// Per-package accumulated results.
#[derive(Default)]
struct PackageResults {
    passed: usize,
    failed: usize,
    skipped: usize,
    /// (test_name, duration, captured output)
    failures: Vec<(String, Duration, String)>,
    /// Per-test output lines, keyed by test name.
    outputs: HashMap<String, Vec<String>>,
}

/// Parse `go test -json` stdout lines, accumulate per-package, and emit
/// package-level summaries via the writer.
fn process_go_test_json(reader: impl BufRead, writer: &BlockWriter, verbose: bool) -> TestJsonResult {
    let mut all_passed = true;
    let mut had_events = false;
    let mut packages: HashMap<String, PackageResults> = HashMap::new();

    for line in reader.lines() {
        let Ok(line) = line else { break };
        let Ok(event) = serde_json::from_str::<GoTestEvent>(&line) else {
            continue;
        };

        match event.action.as_str() {
            "output" if event.test.is_some() => {
                let test_name = event.test.as_ref().unwrap();
                if let Some(text) = &event.output {
                    let trimmed = text.trim_end_matches('\n');
                    if !trimmed.starts_with("--- ") && !trimmed.starts_with("=== ") {
                        packages
                            .entry(event.package.clone())
                            .or_default()
                            .outputs
                            .entry(test_name.clone())
                            .or_default()
                            .push(trimmed.to_owned());
                    }
                }
            }
            "pass" if event.test.is_some() => {
                let test_name = event.test.as_ref().unwrap();
                let pkg = packages.entry(event.package.clone()).or_default();
                pkg.passed += 1;
                if verbose {
                    let duration = Duration::from_secs_f64(event.elapsed.unwrap_or(0.0));
                    writer.test_passed(&event.package, test_name, duration);
                }
                pkg.outputs.remove(test_name);
            }
            "fail" if event.test.is_some() => {
                let test_name = event.test.as_ref().unwrap();
                let pkg = packages.entry(event.package.clone()).or_default();
                let duration = Duration::from_secs_f64(event.elapsed.unwrap_or(0.0));
                let output = pkg
                    .outputs
                    .remove(test_name)
                    .map(|lines| lines.join("\n"))
                    .unwrap_or_default();
                pkg.failed += 1;
                if verbose {
                    writer.test_failed(&event.package, test_name, duration, &output);
                } else {
                    pkg.failures.push((test_name.clone(), duration, output));
                }
            }
            "skip" if event.test.is_some() => {
                let pkg = packages.entry(event.package.clone()).or_default();
                pkg.skipped += 1;
                if verbose {
                    let test_name = event.test.as_ref().unwrap();
                    writer.test_skipped(&event.package, test_name);
                }
            }
            "pass" if event.test.is_none() => {
                had_events = true;
                let duration = Duration::from_secs_f64(event.elapsed.unwrap_or(0.0));
                let pkg = packages.remove(&event.package).unwrap_or_default();
                let total = pkg.passed + pkg.failed + pkg.skipped;
                if total == 0 {
                    continue;
                }
                if !verbose {
                    writer.test_suite_passed(&event.package, duration, pkg.passed, pkg.skipped);
                }
            }
            "fail" if event.test.is_none() => {
                had_events = true;
                all_passed = false;
                let duration = Duration::from_secs_f64(event.elapsed.unwrap_or(0.0));
                let pkg = packages.remove(&event.package).unwrap_or_default();
                if !verbose {
                    writer.test_suite_failed(&event.package, duration, pkg.passed, pkg.failed, &pkg.failures);
                }
            }
            "skip" if event.test.is_none() => {
                had_events = true;
                packages.remove(&event.package);
                if !verbose {
                    writer.test_suite_skipped(&event.package);
                }
            }
            _ => {}
        }
    }

    TestJsonResult {
        passed: all_passed,
        had_events,
    }
}

pub struct GoTestResource;

impl Resource for GoTestResource {
    type State = GoTestState;
    type Inputs = GoTestInputs;
    type Outputs = GoTestOutputs;

    fn name(&self) -> &str {
        "test"
    }

    fn kind(&self) -> ResourceKind {
        ResourceKind::Test
    }

    fn resolve(&self, inputs: &GoTestInputs) -> Result<Vec<ResolvedFile>, BoxError> {
        super::resolve_go_inputs(&inputs.package, true)
    }

    fn plan(&self, inputs: &GoTestInputs, prior_state: Option<&GoTestState>) -> Result<PlanResult, BoxError> {
        let description = format!("go test {}", inputs.package);

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
        inputs: &GoTestInputs,
        _prior_state: Option<&GoTestState>,
        writer: &BlockWriter,
    ) -> Result<ApplyResult<GoTestState, GoTestOutputs>, BoxError> {
        let mut args = vec!["test".to_owned(), "-json".to_owned()];
        args.extend(inputs.flags.iter().cloned());
        args.push(inputs.package.clone());

        let mut cmd = Command::new("go");
        cmd.args(&args).stdout(Stdio::piped()).stderr(Stdio::piped());
        inputs.env.apply_to(&mut cmd);

        let mut child = cmd.spawn().map_err(|e| format!("failed to execute `go test`: {e}"))?;

        let stdout = child.stdout.take();
        let stderr = child.stderr.take();

        let mut result = TestJsonResult {
            passed: true,
            had_events: false,
        };
        std::thread::scope(|s| {
            let verbose = inputs.verbose;
            let json_handle =
                stdout.map(|out| s.spawn(move || process_go_test_json(BufReader::new(out), writer, verbose)));
            if let Some(err) = stderr {
                s.spawn(|| writer.pipe_stderr(BufReader::new(err)));
            }
            if let Some(handle) = json_handle {
                result = handle.join().unwrap_or(TestJsonResult {
                    passed: false,
                    had_events: false,
                });
            }
        });

        let status = child.wait().map_err(|e| format!("failed to wait for `go test`: {e}"))?;

        // Trust the JSON stream if we got test events; fall back to exit code otherwise
        // (e.g. compile errors produce no test events).
        let passed = if result.had_events {
            result.passed
        } else {
            status.success()
        };

        Ok(ApplyResult {
            outputs: GoTestOutputs { passed },
            state: Some(GoTestState {
                package: inputs.package.clone(),
                flags: inputs.flags.clone(),
                env: inputs.env.clone(),
            }),
        })
    }

    fn destroy(&self, _prior_state: &GoTestState, _writer: &BlockWriter) -> Result<(), BoxError> {
        Ok(())
    }

    fn refresh(&self, prior_state: &GoTestState) -> Result<ApplyResult<GoTestState, GoTestOutputs>, BoxError> {
        Ok(ApplyResult {
            outputs: GoTestOutputs { passed: true },
            state: Some(prior_state.clone()),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::output::Output;

    #[test]
    fn resource_kind_is_test() {
        let resource = GoTestResource;
        assert_eq!(Resource::kind(&resource), ResourceKind::Test);
    }

    #[test]
    fn plan_create_when_no_prior_state() {
        let inputs = GoTestInputs {
            package: "./...".into(),
            flags: vec![],
            verbose: false,
            env: GoEnv::default(),
        };
        let resource = GoTestResource;
        let result = Resource::plan(&resource, &inputs, None).unwrap();
        assert_eq!(result.action, PlanAction::Create);
    }

    #[test]
    fn dyn_resource_deserializes_inputs() {
        use crate::provider::DynResource;
        use crate::value::{Map, Value};

        let mut inputs = Map::new();
        inputs.insert("package".into(), Value::Str("./...".into()));
        inputs.insert(
            "flags".into(),
            Value::List(vec![
                Value::Str("-timeout".into()),
                Value::Str("30s".into()),
                Value::Str("-race".into()),
            ]),
        );

        let resource: Box<dyn DynResource> = Box::new(GoTestResource);
        let result = resource.plan(&inputs, None).unwrap();
        assert_eq!(result.action, PlanAction::Create);
    }

    #[test]
    fn plan_update_when_flags_changed() {
        let inputs = GoTestInputs {
            package: "./...".into(),
            flags: vec!["-v".into()],
            verbose: false,
            env: GoEnv::default(),
        };
        let prior = GoTestState {
            package: "./...".into(),
            flags: vec![],
            env: GoEnv::default(),
        };
        let resource = GoTestResource;
        let result = Resource::plan(&resource, &inputs, Some(&prior)).unwrap();
        assert_eq!(result.action, PlanAction::Update);
    }

    #[test]
    fn process_json_package_pass() {
        let json = concat!(
            r#"{"Action":"pass","Package":"example.com/pkg","Test":"TestFoo","Elapsed":0.1}"#,
            "\n",
            r#"{"Action":"pass","Package":"example.com/pkg","Elapsed":0.123}"#,
            "\n",
        );
        let out = Output::new(&[]);
        let writer = out.writer("test");
        let result = process_go_test_json(std::io::Cursor::new(json), &writer, false);
        assert!(result.passed);
        assert!(result.had_events);
    }

    #[test]
    fn process_json_package_fail() {
        let json = concat!(
            r#"{"Action":"output","Package":"example.com/pkg","Test":"TestBar","Output":"    expected 1, got 2\n"}"#,
            "\n",
            r#"{"Action":"fail","Package":"example.com/pkg","Test":"TestBar","Elapsed":0.05}"#,
            "\n",
            r#"{"Action":"fail","Package":"example.com/pkg","Elapsed":0.06}"#,
            "\n",
        );
        let out = Output::new(&[]);
        let writer = out.writer("test");
        let result = process_go_test_json(std::io::Cursor::new(json), &writer, false);
        assert!(!result.passed);
        assert!(result.had_events);
    }

    #[test]
    fn process_json_package_skip() {
        let json = concat!(
            r#"{"Action":"skip","Package":"example.com/pkg","Test":"TestSkipped"}"#,
            "\n",
            r#"{"Action":"skip","Package":"example.com/pkg","Elapsed":0}"#,
            "\n",
        );
        let out = Output::new(&[]);
        let writer = out.writer("test");
        let result = process_go_test_json(std::io::Cursor::new(json), &writer, false);
        assert!(result.passed);
        assert!(result.had_events);
    }

    #[test]
    fn process_json_mixed_packages() {
        let json = concat!(
            r#"{"Action":"pass","Package":"example.com/a","Test":"TestA","Elapsed":0.01}"#,
            "\n",
            r#"{"Action":"pass","Package":"example.com/a","Elapsed":0.02}"#,
            "\n",
            r#"{"Action":"fail","Package":"example.com/b","Test":"TestB","Elapsed":0.03}"#,
            "\n",
            r#"{"Action":"fail","Package":"example.com/b","Elapsed":0.04}"#,
            "\n",
        );
        let out = Output::new(&[]);
        let writer = out.writer("test");
        let result = process_go_test_json(std::io::Cursor::new(json), &writer, false);
        assert!(!result.passed);
    }

    #[test]
    fn process_json_no_events() {
        let out = Output::new(&[]);
        let writer = out.writer("test");
        let result = process_go_test_json(std::io::Cursor::new(""), &writer, false);
        assert!(result.passed);
        assert!(!result.had_events);
    }
}
