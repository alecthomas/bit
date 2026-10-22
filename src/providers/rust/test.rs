use std::collections::BTreeMap;
use std::io::{BufRead, BufReader};
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::file_tracker::FileTracker;
use crate::output::BlockWriter;
use crate::provider::{
    Ambient, ApplyResult, Block, BoxError, CachePolicy, Changes, PlanAction, PlanResult, Resource, ResourceKind,
};
use crate::sha256::SHA256;

use super::{CargoCommand, RustEnv, RustFeatures};
use crate::providers::ReportMode;

/// Run Rust tests
#[derive(Debug, Deserialize, bit_derive::Schema)]
pub struct RustTestInputs {
    /// Package to test (-p flag)
    #[serde(default)]
    pub package: Option<String>,
    /// Extra flags passed to cargo test
    #[serde(default)]
    pub flags: Vec<String>,
    /// What to show: a line per test binary (`suites`), a line per test
    /// (`tests`), or a line per test plus cargo's output as it arrives
    /// (`verbose`). Outside `verbose`, cargo's diagnostics are shown only on
    /// failure.
    #[serde(default)]
    pub report: ReportMode,
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
    // Cargo reports no per-test durations, so test events carry none.
    TestPassed {
        name: String,
    },
    TestFailed {
        name: String,
    },
    TestIgnored {
        name: String,
    },
    SuiteResult {
        passed: usize,
        failed: usize,
        ignored: usize,
        /// From "finished in Ns", if present.
        duration: Option<Duration>,
    },
    Other,
}

/// Parse a single line of `cargo test` output.
fn parse_test_line(line: &str) -> TestEvent {
    let trimmed = line.trim();
    if let Some(rest) = trimmed.strip_prefix("test ") {
        if let Some(name) = rest.strip_suffix(" ... ok") {
            return TestEvent::TestPassed { name: name.to_owned() };
        }
        if let Some(name) = rest.strip_suffix(" ... FAILED") {
            return TestEvent::TestFailed { name: name.to_owned() };
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
    let mut duration = None;

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
            duration = t.parse().ok().and_then(|secs| Duration::try_from_secs_f64(secs).ok());
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
    diagnostics: Vec<String>,
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

fn running_suite_name(rest: &str) -> String {
    let target = rest.split('(').next().unwrap_or(rest).trim();
    let Some(source) = target.strip_prefix("unittests ") else {
        return target.to_owned();
    };
    // Cargo's executable name identifies the crate when workspace members share src/lib.rs.
    let Some(executable) = rest
        .strip_prefix(target)
        .and_then(|suffix| suffix.trim().strip_prefix('('))
    else {
        return source.to_owned();
    };
    let Some(crate_name) = executable
        .trim_end_matches(')')
        .rsplit(['/', '\\'])
        .next()
        .and_then(|filename| filename.trim_end_matches(".exe").rsplit_once('-'))
        .map(|(name, _)| name)
    else {
        return source.to_owned();
    };
    format!("{crate_name} ({source})")
}

/// Process `cargo test` output, parsing test events and forwarding to the writer.
///
/// Consumes lines lazily from `lines`, so callers that feed it from a live
/// pipe (see `apply`) will see events emitted as each suite finishes — not
/// all at once at the end. The boolean marks stderr so Cargo diagnostics can
/// be retained until its exit status is known; in verbose mode they stream
/// instead.
fn process_test_output(
    lines: impl IntoIterator<Item = (String, bool)>,
    writer: &BlockWriter,
    mode: ReportMode,
) -> TestResult {
    let verbose = mode == ReportMode::Verbose;
    let per_test = mode.per_test();
    let mut all_passed = true;
    let mut diagnostics = Vec::new();
    let mut current_suite = String::new();
    let mut failure_lines: Vec<String> = Vec::new();
    // Failing tests in the current suite. Cargo prints their output after
    // every test in the suite, so their per-test lines wait for it.
    let mut failed_tests: Vec<String> = Vec::new();
    let mut in_failures_section = false;
    let log = |line: &str, stderr: bool| {
        if stderr {
            writer.stderr_line(line);
        } else {
            writer.line(line);
        }
    };

    for (line, stderr) in lines {
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

        // Skip cargo progress lines unless streaming everything.
        if trimmed.starts_with("Finished ")
            || trimmed.starts_with("Compiling ")
            || trimmed.starts_with("Downloading ")
            || trimmed.starts_with("Downloaded ")
        {
            if verbose {
                log(&line, stderr);
            }
            continue;
        }

        // Track which test binary is running (these come from stderr).
        if let Some(rest) = trimmed.strip_prefix("Running ") {
            current_suite = running_suite_name(rest);
            continue;
        }
        if let Some(rest) = trimmed.strip_prefix("Doc-tests ") {
            current_suite = format!("doc-tests {rest}");
            continue;
        }

        match parse_test_line(&line) {
            TestEvent::TestPassed { name } => {
                if per_test {
                    writer.test_passed(&current_suite, &name, None);
                }
            }
            TestEvent::TestFailed { name } => failed_tests.push(name),
            TestEvent::TestIgnored { name } => {
                if per_test {
                    writer.test_skipped(&current_suite, &name);
                }
            }
            TestEvent::SuiteResult {
                passed,
                failed,
                ignored,
                duration,
            } => {
                let mut outputs = parse_failure_output(&failure_lines);
                if per_test {
                    for name in failed_tests.drain(..) {
                        let output = outputs
                            .iter()
                            .position(|(n, _)| *n == name)
                            .map(|i| outputs.swap_remove(i).1)
                            .unwrap_or_default();
                        writer.test_failed(&current_suite, &name, None, &output);
                    }
                }
                failed_tests.clear();
                if failed > 0 {
                    all_passed = false;
                    if !per_test {
                        let failures: Vec<_> = outputs.into_iter().map(|(name, output)| (name, None, output)).collect();
                        writer.test_suite_failed(&current_suite, duration, passed, failed, &failures);
                    }
                } else if !per_test {
                    writer.test_suite_passed(&current_suite, duration, passed, ignored);
                }
                failure_lines.clear();
            }
            TestEvent::SuiteStart => {}
            TestEvent::Other if verbose => log(&line, stderr),
            TestEvent::Other => {
                if stderr {
                    diagnostics.push(line);
                }
            }
        }
    }

    TestResult {
        passed: all_passed,
        diagnostics,
    }
}

pub struct RustTestResource {
    pub tracker: Arc<FileTracker>,
}

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

    fn ambient(&self, inputs: &Block<RustTestInputs>) -> Vec<Ambient> {
        super::build_ambient(&inputs.env, &[])
    }

    fn resolve(&self, inputs: &Block<RustTestInputs>) -> Result<BTreeMap<String, SHA256>, BoxError> {
        let tracker = &*self.tracker;
        super::resolve_rust_inputs(inputs.package.as_deref(), tracker)
    }

    fn plan(
        &self,
        inputs: &Block<RustTestInputs>,
        prior_state: Option<&RustTestState>,
    ) -> Result<PlanResult, BoxError> {
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
        inputs: &Block<RustTestInputs>,
        _prior_state: Option<&RustTestState>,
        _changes: &Changes,
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
        // compiler diagnostics; stdout carries results. Both are interleaved
        // by arrival time, so each line retains its source.
        let (tx, rx) = std::sync::mpsc::channel::<(String, bool)>();
        let result = std::thread::scope(|s| {
            if let Some(out) = stdout {
                let tx = tx.clone();
                s.spawn(move || {
                    for line in BufReader::new(out).lines().map_while(Result::ok) {
                        let _ = tx.send((line, false));
                    }
                });
            }
            if let Some(err) = stderr {
                let tx = tx.clone();
                s.spawn(move || {
                    for line in BufReader::new(err).lines().map_while(Result::ok) {
                        let _ = tx.send((line, true));
                    }
                });
            }
            // Drop the original sender so `rx` closes once both pipe threads
            // have finished and dropped their clones.
            drop(tx);
            process_test_output(rx, writer, inputs.report)
        });

        let status = child
            .wait()
            .map_err(|e| format!("failed to wait for `{}`: {e}", cargo.display()))?;

        if !status.success() {
            writer.stderr_details(&result.diagnostics);
        }
        let passed = status.success() && result.passed;

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

    fn destroy(&self, _prior_state: &Block<RustTestState>, _writer: &BlockWriter) -> Result<(), BoxError> {
        Ok(())
    }

    fn cache_policy(&self, _inputs: &Block<RustTestInputs>) -> CachePolicy {
        CachePolicy::Shared { version: 1 }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::output::Output;

    fn make_resource() -> RustTestResource {
        RustTestResource {
            tracker: Arc::new(FileTracker::default()),
        }
    }

    #[test]
    fn resource_kind_is_test() {
        assert_eq!(Resource::kind(&make_resource()), ResourceKind::Test);
    }

    #[test]
    fn plan_create_when_no_prior_state() {
        let inputs = Block::new(
            None,
            RustTestInputs {
                package: None,
                flags: vec![],
                report: ReportMode::default(),
                features: RustFeatures::default(),
                env: RustEnv::default(),
            },
        );
        let result = Resource::plan(&make_resource(), &inputs, None).unwrap();
        assert_eq!(result.action, PlanAction::Create);
        assert_eq!(result.description, "cargo test");
    }

    #[test]
    fn plan_create_with_package() {
        let inputs = Block::new(
            None,
            RustTestInputs {
                package: Some("my-crate".into()),
                flags: vec![],
                report: ReportMode::default(),
                features: RustFeatures::default(),
                env: RustEnv::default(),
            },
        );
        let result = Resource::plan(&make_resource(), &inputs, None).unwrap();
        assert_eq!(result.description, "cargo test -p my-crate");
    }

    #[test]
    fn plan_none_when_unchanged() {
        let inputs = Block::new(
            None,
            RustTestInputs {
                package: None,
                flags: vec![],
                report: ReportMode::default(),
                features: RustFeatures::default(),
                env: RustEnv::default(),
            },
        );
        let prior = RustTestState {
            package: None,
            flags: vec![],
            features: RustFeatures::default(),
            env: RustEnv::default(),
        };
        let result = Resource::plan(&make_resource(), &inputs, Some(&prior)).unwrap();
        assert_eq!(result.action, PlanAction::None);
    }

    #[test]
    fn plan_update_when_flags_changed() {
        let inputs = Block::new(
            None,
            RustTestInputs {
                package: None,
                flags: vec!["--no-fail-fast".into()],
                report: ReportMode::default(),
                features: RustFeatures::default(),
                env: RustEnv::default(),
            },
        );
        let prior = RustTestState {
            package: None,
            flags: vec![],
            features: RustFeatures::default(),
            env: RustEnv::default(),
        };
        let result = Resource::plan(&make_resource(), &inputs, Some(&prior)).unwrap();
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
        let result = process_test_output(
            output.lines().map(|line| (line.into(), false)),
            &writer,
            ReportMode::Suites,
        );
        assert!(result.passed);
        assert!(result.diagnostics.is_empty());
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
        let result = process_test_output(
            output.lines().map(|line| (line.into(), false)),
            &writer,
            ReportMode::Suites,
        );
        assert!(!result.passed);
        assert!(result.diagnostics.is_empty());
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
        let result = process_test_output(
            output.lines().map(|line| (line.into(), false)),
            &writer,
            ReportMode::Suites,
        );
        assert!(!result.passed);
        assert!(result.diagnostics.is_empty());
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
    fn running_suite_names_distinguish_workspace_crates() {
        assert_eq!(
            running_suite_name("unittests src/lib.rs (target/debug/deps/bit-a1b2c3)"),
            "bit (src/lib.rs)"
        );
        assert_eq!(
            running_suite_name("unittests src/lib.rs (target/debug/deps/bit_derive-d4e5f6)"),
            "bit_derive (src/lib.rs)"
        );
        assert_eq!(
            running_suite_name("tests/integration.rs (target/debug/deps/integration-f7a8b9)"),
            "tests/integration.rs"
        );
    }

    #[test]
    fn process_output_no_events() {
        let out = Output::new(&[]);
        let writer = out.writer("test");
        let result = process_test_output(std::iter::empty::<(String, bool)>(), &writer, ReportMode::Suites);
        assert!(result.passed);
        assert!(result.diagnostics.is_empty());
    }

    #[test]
    fn process_output_retains_compiler_diagnostics_after_passing_suite() {
        let lines = [
            ("Running unittests src/lib.rs (target/debug/deps/example)".into(), true),
            (
                "test result: ok. 1 passed; 0 failed; 0 ignored; finished in 0.1s".into(),
                false,
            ),
            ("   Compiling broken-crate v0.1.0".into(), true),
            ("error[E0283]: type annotations needed".into(), true),
            (" --> src/lib.rs:318:13".into(), true),
            (
                "error: could not compile `broken-crate` due to 1 previous error".into(),
                true,
            ),
        ];
        let out = Output::silent();
        let writer = out.writer("test");
        let result = process_test_output(lines.clone(), &writer, ReportMode::Suites);
        assert!(result.passed);
        assert_eq!(
            result.diagnostics,
            [
                "error[E0283]: type annotations needed",
                " --> src/lib.rs:318:13",
                "error: could not compile `broken-crate` due to 1 previous error",
            ]
        );
        // Verbose mode streams the same diagnostics instead of retaining them.
        let result = process_test_output(lines, &writer, ReportMode::Verbose);
        assert!(result.diagnostics.is_empty());
    }
}
