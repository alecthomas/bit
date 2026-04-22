use std::io::{BufRead, BufReader};
use std::process::Stdio;
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::output::BlockWriter;
use crate::provider::{ApplyResult, BoxError, PlanAction, PlanResult, ResolvedFile, Resource, ResourceKind};

use super::{CargoCommand, RustEnv, RustFeatures};

/// Run Rust tests
#[derive(Debug, Deserialize, bit_derive::Schema)]
pub struct RustTestInputs {
    /// Package to test (-p flag)
    #[serde(default)]
    pub package: Option<String>,
    /// Extra flags passed to cargo test
    #[serde(default)]
    pub flags: Vec<String>,
    /// Show individual test results
    #[serde(default)]
    pub verbose: bool,
    #[serde(flatten)]
    pub features: RustFeatures,
    #[serde(flatten)]
    pub env: RustEnv,
}

/// Outputs from a `rust.test` block.
#[derive(Debug, Serialize, bit_derive::Schema)]
pub struct RustTestOutputs {
    /// Whether the check passed
    pub passed: bool,
}

/// Persisted state for a `rust.test` block.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RustTestState {
    pub package: Option<String>,
    pub flags: Vec<String>,
    #[serde(flatten)]
    pub features: RustFeatures,
    #[serde(flatten)]
    pub env: RustEnv,
}

/// A parsed line of `cargo test` output for structured reporting.
#[derive(Debug)]
enum TestEvent {
    SuiteStart,
    TestPassed {
        name: String,
        duration: Duration,
    },
    TestFailed {
        name: String,
        duration: Duration,
    },
    TestIgnored {
        name: String,
    },
    SuiteResult {
        passed: usize,
        failed: usize,
        ignored: usize,
        duration: Duration,
    },
    Other,
}

/// Parse a single line of `cargo test` output.
fn parse_test_line(line: &str) -> TestEvent {
    let trimmed = line.trim();
    if let Some(rest) = trimmed.strip_prefix("test ") {
        if let Some(name) = rest.strip_suffix(" ... ok") {
            return TestEvent::TestPassed {
                name: name.to_owned(),
                duration: Duration::ZERO,
            };
        }
        if let Some(name) = rest.strip_suffix(" ... FAILED") {
            return TestEvent::TestFailed {
                name: name.to_owned(),
                duration: Duration::ZERO,
            };
        }
        if let Some(name) = rest.strip_suffix(" ... ignored") {
            return TestEvent::TestIgnored { name: name.to_owned() };
        }
    }
    if let Some(rest) = trimmed.strip_prefix("running ")
        && rest.contains(" test")
    {
        return TestEvent::SuiteStart;
    }
    if let Some(rest) = trimmed.strip_prefix("test result: ") {
        return parse_result_line(rest);
    }
    TestEvent::Other
}

/// Parse "ok. N passed; N failed; N ignored; finished in Ns"
fn parse_result_line(rest: &str) -> TestEvent {
    // Strip the leading "ok." or "FAILED." status prefix.
    let rest = rest
        .strip_prefix("ok.")
        .or_else(|| rest.strip_prefix("FAILED."))
        .unwrap_or(rest);

    let mut passed = 0;
    let mut failed = 0;
    let mut ignored = 0;
    let mut duration = Duration::ZERO;

    for part in rest.split(';') {
        let part = part.trim();
        if let Some(n) = part.strip_suffix(" passed") {
            passed = n.trim().parse().unwrap_or(0);
        } else if let Some(n) = part.strip_suffix(" failed") {
            failed = n.trim().parse().unwrap_or(0);
        } else if let Some(n) = part.strip_suffix(" ignored") {
            ignored = n.trim().parse().unwrap_or(0);
        } else if let Some(t) = part.strip_prefix("finished in ") {
            let t = t.trim_end_matches('s').trim();
            duration = Duration::from_secs_f64(t.parse().unwrap_or(0.0));
        }
    }

    TestEvent::SuiteResult {
        passed,
        failed,
        ignored,
        duration,
    }
}

struct TestResult {
    passed: bool,
    had_events: bool,
}

/// Parse the "failures:" section into per-test output.
/// The section format is:
/// ```text
/// ---- test_name stdout ----
/// <output lines>
/// ---- other_test stdout ----
/// <output lines>
/// ```
fn parse_failure_output(lines: &[String]) -> Vec<(String, String)> {
    let mut results = Vec::new();
    let mut current_name = String::new();
    let mut current_output: Vec<String> = Vec::new();

    for line in lines {
        if let Some(rest) = line.strip_prefix("---- ")
            && let Some(name) = rest.strip_suffix(" stdout ----")
        {
            // Save previous test's output if any.
            if !current_name.is_empty() {
                results.push((current_name.clone(), current_output.join("\n")));
                current_output.clear();
            }
            current_name = name.to_owned();
        } else if !current_name.is_empty() {
            current_output.push(line.clone());
        }
    }
    if !current_name.is_empty() {
        results.push((current_name, current_output.join("\n")));
    }
    results
}

/// Process `cargo test` output, parsing test events and forwarding to the writer.
///
/// Consumes lines lazily from `lines`, so callers that feed it from a live
/// pipe (see `apply`) will see events emitted as each suite finishes — not
/// all at once at the end. The BufRead-based caller path uses
/// `reader.lines().map_while(Result::ok)` to adapt.
fn process_test_output(lines: impl IntoIterator<Item = String>, writer: &BlockWriter, verbose: bool) -> TestResult {
    let mut all_passed = true;
    let mut had_events = false;
    let mut current_suite = String::new();
    let mut failure_lines: Vec<String> = Vec::new();
    let mut in_failures_section = false;

    for line in lines {
        // Detect the "failures:" section that cargo test prints with full output.
        if line.trim() == "failures:" {
            in_failures_section = true;
            continue;
        }
        if in_failures_section {
            if line.trim() == "failures:" {
                // Second "failures:" header lists just names; skip.
                in_failures_section = false;
                continue;
            }
            if line.starts_with("test result:") {
                in_failures_section = false;
                // Fall through to parse this line.
            } else {
                failure_lines.push(line);
                continue;
            }
        }

        let trimmed = line.trim();

        // Skip cargo progress lines.
        if trimmed.starts_with("Finished ")
            || trimmed.starts_with("Compiling ")
            || trimmed.starts_with("Downloading ")
            || trimmed.starts_with("Downloaded ")
        {
            continue;
        }

        // Track which test binary is running (these come from stderr).
        // Extract a clean name like "src/lib.rs" or "tests/integration.rs".
        if let Some(rest) = trimmed.strip_prefix("Running ") {
            // "Running unittests src/lib.rs (target/debug/deps/bit-abc123)"
            // or "Running tests/foo.rs (target/debug/deps/foo-abc123)"
            current_suite = rest
                .split('(')
                .next()
                .unwrap_or(rest)
                .trim()
                .strip_prefix("unittests ")
                .unwrap_or(rest.split('(').next().unwrap_or(rest).trim())
                .to_owned();
            continue;
        }
        if let Some(rest) = trimmed.strip_prefix("Doc-tests ") {
            current_suite = format!("doc-tests {rest}");
            continue;
        }

        match parse_test_line(&line) {
            TestEvent::TestPassed { name, duration } => {
                had_events = true;
                if verbose {
                    writer.test_passed(&current_suite, &name, duration);
                }
            }
            TestEvent::TestFailed { name, duration } => {
                had_events = true;
                if verbose {
                    writer.test_failed(&current_suite, &name, duration, "");
                }
            }
            TestEvent::TestIgnored { name } => {
                had_events = true;
                if verbose {
                    writer.test_skipped(&current_suite, &name);
                }
            }
            TestEvent::SuiteResult {
                passed,
                failed,
                ignored,
                duration,
            } => {
                had_events = true;
                if failed > 0 {
                    all_passed = false;
                    if !verbose {
                        let failures: Vec<_> = parse_failure_output(&failure_lines)
                            .into_iter()
                            .map(|(name, output)| (name, Duration::ZERO, output))
                            .collect();
                        writer.test_suite_failed(&current_suite, duration, passed, failed, &failures);
                    }
                } else if !verbose {
                    writer.test_suite_passed(&current_suite, duration, passed, ignored);
                }
                failure_lines.clear();
            }
            TestEvent::SuiteStart => {}
            TestEvent::Other => {}
        }
    }

    TestResult {
        passed: all_passed,
        had_events,
    }
}

pub struct RustTestResource;

fn test_command(inputs: &RustTestInputs) -> CargoCommand {
    let mut cargo = inputs.env.cargo("test");
    if let Some(pkg) = &inputs.package {
        cargo.arg2("-p", pkg);
    }
    cargo.features(&inputs.features).extra_flags(&inputs.flags);
    cargo
}

impl Resource for RustTestResource {
    type State = RustTestState;
    type Inputs = RustTestInputs;
    type Outputs = RustTestOutputs;

    fn name(&self) -> &str {
        "test"
    }

    fn kind(&self) -> ResourceKind {
        ResourceKind::Test
    }

    fn resolve(&self, _inputs: &RustTestInputs) -> Result<Vec<ResolvedFile>, BoxError> {
        super::resolve_rust_inputs()
    }

    fn plan(&self, inputs: &RustTestInputs, prior_state: Option<&RustTestState>) -> Result<PlanResult, BoxError> {
        let description = test_command(inputs).display();

        let Some(prior) = prior_state else {
            return Ok(PlanResult {
                action: PlanAction::Create,
                description,
                reason: None,
            });
        };

        let action = if prior.package != inputs.package
            || prior.flags != inputs.flags
            || prior.features != inputs.features
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
        inputs: &RustTestInputs,
        _prior_state: Option<&RustTestState>,
        writer: &BlockWriter,
    ) -> Result<ApplyResult<RustTestState, RustTestOutputs>, BoxError> {
        let cargo = test_command(inputs);
        let mut child = cargo
            .command()
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| format!("failed to execute `{}`: {e}", cargo.display()))?;

        let stdout = child.stdout.take();
        let stderr = child.stderr.take();

        // Stream lines from both pipes through a channel so the parser can
        // emit `test_suite_passed` / etc. events as each suite finishes,
        // giving the live region something to show during a long test run.
        // Stderr carries suite headers ("Running ...", "Doc-tests ...") and
        // stdout carries results; both are interleaved by arrival time.
        let (tx, rx) = std::sync::mpsc::channel::<String>();
        let result = std::thread::scope(|s| {
            if let Some(out) = stdout {
                let tx = tx.clone();
                s.spawn(move || {
                    for line in BufReader::new(out).lines().map_while(Result::ok) {
                        let _ = tx.send(line);
                    }
                });
            }
            if let Some(err) = stderr {
                let tx = tx.clone();
                s.spawn(move || {
                    for line in BufReader::new(err).lines().map_while(Result::ok) {
                        let _ = tx.send(line);
                    }
                });
            }
            // Drop the original sender so `rx` closes once both pipe threads
            // have finished and dropped their clones.
            drop(tx);
            process_test_output(rx, writer, inputs.verbose)
        });

        let status = child
            .wait()
            .map_err(|e| format!("failed to wait for `{}`: {e}", cargo.display()))?;

        let passed = if result.had_events {
            result.passed
        } else {
            status.success()
        };

        Ok(ApplyResult {
            outputs: RustTestOutputs { passed },
            state: Some(RustTestState {
                package: inputs.package.clone(),
                flags: inputs.flags.clone(),
                features: inputs.features.clone(),
                env: inputs.env.clone(),
            }),
        })
    }

    fn destroy(&self, _prior_state: &RustTestState, _writer: &BlockWriter) -> Result<(), BoxError> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::output::Output;

    #[test]
    fn resource_kind_is_test() {
        assert_eq!(Resource::kind(&RustTestResource), ResourceKind::Test);
    }

    #[test]
    fn plan_create_when_no_prior_state() {
        let inputs = RustTestInputs {
            package: None,
            flags: vec![],
            verbose: false,
            features: RustFeatures::default(),
            env: RustEnv::default(),
        };
        let result = Resource::plan(&RustTestResource, &inputs, None).unwrap();
        assert_eq!(result.action, PlanAction::Create);
        assert_eq!(result.description, "cargo test");
    }

    #[test]
    fn plan_create_with_package() {
        let inputs = RustTestInputs {
            package: Some("my-crate".into()),
            flags: vec![],
            verbose: false,
            features: RustFeatures::default(),
            env: RustEnv::default(),
        };
        let result = Resource::plan(&RustTestResource, &inputs, None).unwrap();
        assert_eq!(result.description, "cargo test -p my-crate");
    }

    #[test]
    fn plan_none_when_unchanged() {
        let inputs = RustTestInputs {
            package: None,
            flags: vec![],
            verbose: false,
            features: RustFeatures::default(),
            env: RustEnv::default(),
        };
        let prior = RustTestState {
            package: None,
            flags: vec![],
            features: RustFeatures::default(),
            env: RustEnv::default(),
        };
        let result = Resource::plan(&RustTestResource, &inputs, Some(&prior)).unwrap();
        assert_eq!(result.action, PlanAction::None);
    }

    #[test]
    fn plan_update_when_flags_changed() {
        let inputs = RustTestInputs {
            package: None,
            flags: vec!["--no-fail-fast".into()],
            verbose: false,
            features: RustFeatures::default(),
            env: RustEnv::default(),
        };
        let prior = RustTestState {
            package: None,
            flags: vec![],
            features: RustFeatures::default(),
            env: RustEnv::default(),
        };
        let result = Resource::plan(&RustTestResource, &inputs, Some(&prior)).unwrap();
        assert_eq!(result.action, PlanAction::Update);
    }

    #[test]
    fn parse_test_passed() {
        match parse_test_line("test my_test ... ok") {
            TestEvent::TestPassed { name, .. } => assert_eq!(name, "my_test"),
            other => panic!("expected TestPassed, got {other:?}"),
        }
    }

    #[test]
    fn parse_test_failed() {
        match parse_test_line("test my_test ... FAILED") {
            TestEvent::TestFailed { name, .. } => assert_eq!(name, "my_test"),
            other => panic!("expected TestFailed, got {other:?}"),
        }
    }

    #[test]
    fn parse_test_ignored() {
        match parse_test_line("test my_test ... ignored") {
            TestEvent::TestIgnored { name } => assert_eq!(name, "my_test"),
            other => panic!("expected TestIgnored, got {other:?}"),
        }
    }

    #[test]
    fn parse_result_pass() {
        match parse_test_line("test result: ok. 10 passed; 0 failed; 2 ignored; finished in 1.5s") {
            TestEvent::SuiteResult {
                passed,
                failed,
                ignored,
                ..
            } => {
                assert_eq!(passed, 10);
                assert_eq!(failed, 0);
                assert_eq!(ignored, 2);
            }
            other => panic!("expected SuiteResult, got {other:?}"),
        }
    }

    #[test]
    fn process_output_all_pass() {
        let output = concat!(
            "running 2 tests\n",
            "test foo ... ok\n",
            "test bar ... ok\n",
            "test result: ok. 2 passed; 0 failed; 0 ignored; finished in 0.5s\n",
        );
        let out = Output::new(&[]);
        let writer = out.writer("test");
        let result = process_test_output(output.lines().map(String::from), &writer, false);
        assert!(result.passed);
        assert!(result.had_events);
    }

    #[test]
    fn process_output_with_failure() {
        let output = concat!(
            "running 2 tests\n",
            "test foo ... ok\n",
            "test bar ... FAILED\n",
            "test result: FAILED. 1 passed; 1 failed; 0 ignored; finished in 0.5s\n",
        );
        let out = Output::new(&[]);
        let writer = out.writer("test");
        let result = process_test_output(output.lines().map(String::from), &writer, false);
        assert!(!result.passed);
        assert!(result.had_events);
    }

    #[test]
    fn process_output_with_failure_details() {
        let output = concat!(
            "running 3 tests\n",
            "test foo ... ok\n",
            "test bar ... FAILED\n",
            "test baz ... FAILED\n",
            "\n",
            "failures:\n",
            "\n",
            "---- bar stdout ----\n",
            "assertion failed: expected 1, got 2\n",
            "\n",
            "---- baz stdout ----\n",
            "thread 'baz' panicked\n",
            "\n",
            "failures:\n",
            "    bar\n",
            "    baz\n",
            "\n",
            "test result: FAILED. 1 passed; 2 failed; 0 ignored; finished in 0.5s\n",
        );
        let out = Output::new(&[]);
        let writer = out.writer("test");
        let result = process_test_output(output.lines().map(String::from), &writer, false);
        assert!(!result.passed);
        assert!(result.had_events);
    }

    #[test]
    fn parse_failure_output_splits_per_test() {
        let lines: Vec<String> = vec![
            "---- foo stdout ----".into(),
            "assertion failed".into(),
            "".into(),
            "---- bar stdout ----".into(),
            "thread panicked".into(),
        ];
        let failures = parse_failure_output(&lines);
        assert_eq!(failures.len(), 2);
        assert_eq!(failures[0].0, "foo");
        assert_eq!(failures[0].1, "assertion failed\n");
        assert_eq!(failures[1].0, "bar");
        assert_eq!(failures[1].1, "thread panicked");
    }

    #[test]
    fn parse_failure_output_empty() {
        let failures = parse_failure_output(&[]);
        assert!(failures.is_empty());
    }

    #[test]
    fn process_output_no_events() {
        let out = Output::new(&[]);
        let writer = out.writer("test");
        let result = process_test_output(std::iter::empty::<String>(), &writer, false);
        assert!(result.passed);
        assert!(!result.had_events);
    }
}
