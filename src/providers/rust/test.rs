use crate::process::CommandExt;
use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::file_tracker::FileTracker;
use crate::output::TargetWriter;
use crate::provider::{
    Ambient, ApplyResult, BoxError, CachePolicy, Changes, PlanAction, PlanResult, Resource, ResourceKind, Target,
};
use crate::sha256::SHA256;

use super::{CargoCommand, MemberGraph, RustEnv, RustFeatures};
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
    /// (`verbose`). Outside `verbose`, cargo's diagnostics, and output tests
    /// write past libtest's capture, are shown only on failure.
    #[serde(default)]
    pub report: ReportMode,
    #[serde(flatten)]
    pub features: RustFeatures,
    #[serde(flatten)]
    pub rust_env: RustEnv,
}

/// Outputs from a `rust.test` target.
#[derive(Debug, Serialize, bit_derive::Schema)]
pub struct RustTestOutputs {
    /// Whether the check passed
    pub passed: bool,
}

/// Persisted state for a `rust.test` target.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RustTestState {
    pub package: Option<String>,
    pub flags: Vec<String>,
    #[serde(flatten)]
    pub features: RustFeatures,
    #[serde(flatten)]
    pub rust_env: RustEnv,
    /// The test executables the last run built, from which the next run
    /// finds those its changes cannot affect.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub executables: Vec<TestExecutable>,
}

/// A test executable cargo built, as `--message-format=json` reports it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TestExecutable {
    pub path: PathBuf,
    /// The directory of the package it tests.
    pub package: PathBuf,
    /// Whether it tests a library or binary, whose code the package's other
    /// targets and the packages depending on it can use. An integration test,
    /// bench or example is used by nothing else.
    pub library: bool,
}

/// A cargo `--message-format=json` message among the output lines, which
/// start with its `reason`.
fn cargo_message(line: &str) -> Option<serde_json::Value> {
    line.starts_with("{\"reason\":")
        .then(|| serde_json::from_str(line).ok())
        .flatten()
}

/// The test executable a cargo JSON message reports, if it reports one.
fn test_executable(message: &serde_json::Value) -> Option<TestExecutable> {
    if message.get("reason")?.as_str()? != "compiler-artifact" || !message.pointer("/profile/test")?.as_bool()? {
        return None;
    }
    let kinds = message.pointer("/target/kind")?.as_array()?;
    Some(TestExecutable {
        path: PathBuf::from(message.get("executable")?.as_str()?),
        package: Path::new(message.get("manifest_path")?.as_str()?)
            .parent()?
            .to_path_buf(),
        library: kinds
            .iter()
            .filter_map(serde_json::Value::as_str)
            .any(|kind| !matches!(kind, "test" | "bench" | "example")),
    })
}

/// The source files in rustc's dep-info for `executable`, with relative paths
/// resolved against `workspace_root`, where cargo runs rustc. `None` when
/// there is no dep-info to read.
fn dep_info_sources(executable: &Path, workspace_root: &Path) -> Option<HashSet<PathBuf>> {
    let contents = std::fs::read_to_string(executable.with_extension("d")).ok()?;
    // Every rule lists the same sources, one for each file rustc wrote.
    let (_, sources) = contents
        .lines()
        .filter(|line| !line.starts_with('#'))
        .find_map(|line| line.split_once(": "))?;
    Some(
        dep_info_paths(sources)
            .into_iter()
            .map(|path| workspace_root.join(path))
            .collect(),
    )
}

/// The space-separated paths of a dep-info rule, where `\ ` is a space in a
/// path.
fn dep_info_paths(sources: &str) -> Vec<String> {
    let mut paths = vec![];
    let mut path = String::new();
    let mut chars = sources.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\\' if chars.peek() == Some(&' ') => path.push(chars.next().unwrap_or(' ')),
            ' ' => paths.extend((!path.is_empty()).then(|| std::mem::take(&mut path))),
            c => path.push(c),
        }
    }
    paths.extend((!path.is_empty()).then_some(path));
    paths
}

/// The recorded `executables` none of the `changed` paths can affect, or
/// `None` when every executable must run.
///
/// A path in the dep-info of an integration test, bench or example affects
/// only that executable. A path in a library's or binary's, or in none, such
/// as the package's manifest or a file its tests read at run time, affects
/// the whole package and every member built with it. An executable without
/// dep-info is always affected.
///
/// The dep-info is the previous build's. A file it does not list can only
/// become a source through an edit to a file it does list, or to the
/// manifest, which affects the executable that now compiles it.
fn unaffected(
    executables: &[TestExecutable],
    members: &MemberGraph,
    changed: &BTreeSet<PathBuf>,
    sources: impl Fn(&TestExecutable) -> Option<HashSet<PathBuf>>,
) -> Option<Vec<PathBuf>> {
    let sources: Vec<_> = executables.iter().map(sources).collect();
    let mut affected: Vec<bool> = sources.iter().map(Option::is_none).collect();
    for path in changed {
        if members.is_config(path) {
            return None;
        }
        let package = members.owner(path)?;
        let owners: Vec<usize> = (0..executables.len())
            .filter(|&i| executables[i].package == package && sources[i].as_ref().is_some_and(|s| s.contains(path)))
            .collect();
        let package_wide =
            owners.is_empty() || owners.iter().any(|&i| executables[i].library) || *path == package.join("Cargo.toml");
        if package_wide {
            let dependents: Vec<&Path> = members.dependents(package).collect();
            for (i, executable) in executables.iter().enumerate() {
                affected[i] |= dependents.contains(&executable.package.as_path());
            }
        } else {
            for i in owners {
                affected[i] = true;
            }
        }
    }
    Some(
        executables
            .iter()
            .zip(affected)
            .filter(|(_, affected)| !affected)
            .map(|(executable, _)| executable.path.clone())
            .collect(),
    )
}

/// A runner for cargo to run test executables through, which exits without
/// running those in `skip`.
fn skipping_runner(skip: &[PathBuf]) -> Result<tempfile::TempPath, BoxError> {
    use std::io::Write;
    let quoted: Vec<String> = skip
        .iter()
        .map(|path| {
            let path = path.to_string_lossy();
            shlex::try_quote(&path).map(|quoted| quoted.into_owned())
        })
        .collect::<Result<_, _>>()?;
    let script = format!(
        "#!/bin/sh\n# Generated by bit: skip test executables the changes cannot affect.\n\
         case \"$1\" in\n{}) exit 0 ;;\nesac\nexec \"$@\"\n",
        quoted.join("|")
    );
    let mut file = tempfile::Builder::new()
        .prefix("bit-test-runner-")
        .suffix(".sh")
        .tempfile()?;
    file.write_all(script.as_bytes())?;
    file.as_file()
        .set_permissions(std::os::unix::fs::PermissionsExt::from_mode(0o755))?;
    // Closed before cargo runs it, as executing a file open for writing fails.
    Ok(file.into_temp_path())
}

/// Whether bit can learn the test executables a run builds and skip some of
/// them: it must add `--message-format` and a runner through `--config`, so
/// it cannot when the target's flags set either, or the user configured a
/// runner, since cargo rejects two runners that both match.
fn can_skip(inputs: &Target<RustTestInputs>) -> Result<bool, BoxError> {
    let cargo_flags = inputs.flags.iter().take_while(|flag| *flag != "--");
    if cargo_flags
        .into_iter()
        .any(|flag| flag.starts_with("--message-format") || flag.starts_with("--config"))
    {
        return Ok(false);
    }
    Ok(!super::runner_configured(&selection(inputs))?)
}

fn selection(inputs: &Target<RustTestInputs>) -> super::Selection<'_> {
    super::Selection {
        dir: inputs.dir.as_deref(),
        env: &inputs.env,
        package: inputs.package.as_deref(),
        flags: &inputs.flags,
    }
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

/// Parse a single line of `cargo test` output, as libtest or a harness that
/// mimics it, such as libtest-mimic, prints it.
fn parse_test_line(line: &str) -> TestEvent {
    let trimmed = line.trim();
    if let Some((name, outcome)) = trimmed.strip_prefix("test ").and_then(|rest| rest.rsplit_once(" ... ")) {
        // libtest-mimic pads names to align the outcomes.
        let name = name.trim_end().to_owned();
        match outcome {
            "ok" => return TestEvent::TestPassed { name },
            "FAILED" => return TestEvent::TestFailed { name },
            // libtest appends a reason as `ignored, reason`, libtest-mimic
            // as `ignored (reason)`.
            _ if outcome == "ignored" || outcome.starts_with("ignored,") || outcome.starts_with("ignored (") => {
                return TestEvent::TestIgnored { name };
            }
            _ => {}
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
/// libtest-mimic leaves out ` stdout`.
fn parse_failure_output(lines: &[String]) -> Vec<(String, String)> {
    let mut results = Vec::new();
    let mut current_name = String::new();
    let mut current_output: Vec<String> = Vec::new();

    for line in lines {
        if let Some(rest) = line.strip_prefix("---- ")
            && let Some(header) = rest.strip_suffix(" ----")
        {
            let name = header.strip_suffix(" stdout").unwrap_or(header);
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
/// all at once at the end. `lines` holds cargo's stdout and stderr in the
/// order they were written. Lines it doesn't recognise are retained as
/// diagnostics until Cargo's exit status is known; in verbose mode they
/// stream instead.
fn process_test_output(lines: impl IntoIterator<Item = String>, writer: &TargetWriter, mode: ReportMode) -> TestResult {
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

        // Skip cargo progress lines unless streaming everything.
        if trimmed.starts_with("Finished ")
            || trimmed.starts_with("Compiling ")
            || trimmed.starts_with("Downloading ")
            || trimmed.starts_with("Downloaded ")
        {
            if verbose {
                writer.line(&line);
            }
            continue;
        }

        // Track which test binary is running.
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
            TestEvent::Other if verbose => writer.line(&line),
            // Cargo's diagnostics, and what a test writes to the stdout handle
            // itself or through child processes, which bypasses libtest's
            // capture and so is in no failure section, can explain a failure.
            TestEvent::Other => {
                if !line.trim().is_empty() {
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

/// `cargo test` for `inputs`, with bit's own cargo flags `internal` placed
/// before the target's flags, which can end with `--` and test arguments.
fn test_command(inputs: &Target<RustTestInputs>, internal: &[String]) -> CargoCommand {
    let mut cargo = inputs.rust_env.cargo("test", inputs.dir.as_deref());
    cargo.envs(&inputs.env);
    if let Some(pkg) = &inputs.package {
        cargo.arg2("-p", pkg);
    }
    cargo
        .features(&inputs.features)
        .extra_flags(internal)
        .extra_flags(&inputs.flags);
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

    fn ambient(&self, inputs: &Target<RustTestInputs>) -> Vec<Ambient> {
        super::build_ambient(&inputs.rust_env, inputs.dir.as_deref(), &inputs.env, &[])
    }

    fn resolve(&self, inputs: &Target<RustTestInputs>) -> Result<BTreeMap<String, SHA256>, BoxError> {
        super::resolve_rust_inputs(&selection(inputs), &self.tracker)
    }

    fn plan(
        &self,
        inputs: &Target<RustTestInputs>,
        prior_state: Option<&RustTestState>,
    ) -> Result<PlanResult, BoxError> {
        let description = test_command(inputs, &[]).display();

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
            || prior.rust_env != inputs.rust_env
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
        inputs: &Target<RustTestInputs>,
        prior_state: Option<&RustTestState>,
        changes: &Changes,
        writer: &TargetWriter,
    ) -> Result<ApplyResult<RustTestState, RustTestOutputs>, BoxError> {
        let mut state = RustTestState {
            package: inputs.package.clone(),
            flags: inputs.flags.clone(),
            features: inputs.features.clone(),
            rust_env: inputs.rust_env.clone(),
            executables: vec![],
        };
        let record = can_skip(inputs)?;
        let prior = prior_state.map_or(&[][..], |prior| prior.executables.as_slice());
        let skip = match changes {
            Changes::Paths(changed) if record && !prior.is_empty() => {
                let members = MemberGraph::discover(&selection(inputs))?;
                unaffected(prior, &members, changed, |executable| {
                    dep_info_sources(&executable.path, members.workspace_root())
                })
            }
            _ => None,
        };
        let total = prior.len();
        if let Some(skip) = &skip
            && skip.len() == total
        {
            writer.starting(&format!("0 of {total} test executables affected"));
            state.executables = prior.to_vec();
            return Ok(ApplyResult {
                outputs: RustTestOutputs { passed: true },
                state: Some(state),
            });
        }
        writer.starting(&test_command(inputs, &[]).display());
        if let Some(skip) = &skip {
            writer.line(&format!("{} of {total} test executables affected", total - skip.len()));
        }

        // Cargo still builds every selected package, so features resolve as
        // in a full run and nothing recompiles; the runner skips executables.
        let mut internal = vec![];
        if record {
            internal.push("--message-format=json-render-diagnostics".to_owned());
        }
        let runner = skip
            .as_deref()
            .filter(|skip| !skip.is_empty())
            .map(skipping_runner)
            .transpose()?;
        if let Some(runner) = &runner {
            let path = toml::Value::String(runner.to_string_lossy().into_owned());
            internal.push("--config".to_owned());
            internal.push(format!("target.'cfg(all())'.runner = {path}"));
        }
        let cargo = test_command(inputs, &internal);
        // Cargo writes each suite's header ("Running …", "Doc-tests …") to
        // stderr, and the test binary its results to stdout. Read from two
        // pipes, a header can arrive before the previous suite's results and
        // name them wrongly, so both streams share one pipe, which keeps the
        // order they were written in.
        let (output, output_writer) =
            std::io::pipe().map_err(|e| format!("failed to create a pipe for `{}`: {e}", cargo.display()))?;
        let mut child = {
            let mut cmd = cargo.command();
            cmd.stdout(output_writer.try_clone()?).stderr(output_writer);
            // Dropped once spawned, as the child's are then the only write
            // ends: reading ends when cargo exits.
            cmd.logged_spawn()
                .map_err(|e| format!("failed to execute `{}`: {e}", cargo.display()))?
        };

        // Read as cargo writes, so a suite's line shows as it finishes,
        // giving the live region something to show during a long test run.
        let lines = BufReader::new(output).lines().map_while(Result::ok).filter(|line| {
            let Some(message) = cargo_message(line) else {
                return true;
            };
            state.executables.extend(test_executable(&message));
            false
        });
        let result = process_test_output(lines, writer, inputs.report);

        let status = child
            .wait()
            .map_err(|e| format!("failed to wait for `{}`: {e}", cargo.display()))?;

        if !status.success() {
            writer.stderr_details(&result.diagnostics);
        }
        let passed = status.success() && result.passed;

        Ok(ApplyResult {
            outputs: RustTestOutputs { passed },
            state: Some(state),
        })
    }

    fn destroy(&self, _prior_state: &Target<RustTestState>, _writer: &TargetWriter) -> Result<(), BoxError> {
        Ok(())
    }

    /// The executables run depend on the changes.
    fn announces_start(&self) -> bool {
        true
    }

    fn cache_policy(&self, _inputs: &Target<RustTestInputs>) -> CachePolicy {
        CachePolicy::Shared { version: 2 }
    }

    fn output_keys(&self, inputs: &Target<RustTestInputs>) -> Vec<String> {
        super::lockfile(&selection(inputs)).into_iter().collect()
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
        let inputs = Target::new(
            None,
            RustTestInputs {
                package: None,
                flags: vec![],
                report: ReportMode::default(),
                features: RustFeatures::default(),
                rust_env: RustEnv::default(),
            },
        );
        let result = Resource::plan(&make_resource(), &inputs, None).unwrap();
        assert_eq!(result.action, PlanAction::Create);
        assert_eq!(result.description, "cargo test");
    }

    #[test]
    fn plan_create_with_package() {
        let inputs = Target::new(
            None,
            RustTestInputs {
                package: Some("my-crate".into()),
                flags: vec![],
                report: ReportMode::default(),
                features: RustFeatures::default(),
                rust_env: RustEnv::default(),
            },
        );
        let result = Resource::plan(&make_resource(), &inputs, None).unwrap();
        assert_eq!(result.description, "cargo test -p my-crate");
    }

    #[test]
    fn plan_none_when_unchanged() {
        let inputs = Target::new(
            None,
            RustTestInputs {
                package: None,
                flags: vec![],
                report: ReportMode::default(),
                features: RustFeatures::default(),
                rust_env: RustEnv::default(),
            },
        );
        let prior = RustTestState {
            package: None,
            flags: vec![],
            features: RustFeatures::default(),
            rust_env: RustEnv::default(),
            executables: vec![],
        };
        let result = Resource::plan(&make_resource(), &inputs, Some(&prior)).unwrap();
        assert_eq!(result.action, PlanAction::None);
    }

    #[test]
    fn plan_update_when_flags_changed() {
        let inputs = Target::new(
            None,
            RustTestInputs {
                package: None,
                flags: vec!["--no-fail-fast".into()],
                report: ReportMode::default(),
                features: RustFeatures::default(),
                rust_env: RustEnv::default(),
            },
        );
        let prior = RustTestState {
            package: None,
            flags: vec![],
            features: RustFeatures::default(),
            rust_env: RustEnv::default(),
            executables: vec![],
        };
        let result = Resource::plan(&make_resource(), &inputs, Some(&prior)).unwrap();
        assert_eq!(result.action, PlanAction::Update);
    }

    #[test]
    fn dep_info_paths_unescape_spaces() {
        assert_eq!(
            dep_info_paths("a/src/lib.rs a/src/my\\ mod.rs  /abs/b.rs"),
            ["a/src/lib.rs", "a/src/my mod.rs", "/abs/b.rs"]
        );
    }

    #[test]
    fn test_executables_come_from_test_artifacts() {
        let message = |kind: &str, test: bool| {
            serde_json::json!({
                "reason": "compiler-artifact",
                "manifest_path": "/ws/a/Cargo.toml",
                "target": {"kind": [kind]},
                "profile": {"test": test},
                "executable": "/ws/target/debug/deps/a-0123",
            })
        };
        let executable = |library| TestExecutable {
            path: "/ws/target/debug/deps/a-0123".into(),
            package: "/ws/a".into(),
            library,
        };
        assert_eq!(test_executable(&message("lib", true)), Some(executable(true)));
        assert_eq!(test_executable(&message("test", true)), Some(executable(false)));
        assert_eq!(test_executable(&message("lib", false)), None);
    }

    /// `core` and `app`, which depends on `core`, in `/ws`.
    fn members() -> MemberGraph {
        MemberGraph {
            closures: vec![
                ("/ws/core".into(), vec!["/ws/core".into()]),
                ("/ws/app".into(), vec!["/ws/app".into(), "/ws/core".into()]),
            ],
            config: vec!["/ws/Cargo.toml".into(), "/ws/Cargo.lock".into()],
            workspace_root: "/ws".into(),
        }
    }

    #[test]
    fn unaffected_executables_follow_dep_info_and_dependents() {
        let executable = |name: &str, package: &str, library| TestExecutable {
            path: format!("/ws/target/{name}").into(),
            package: format!("/ws/{package}").into(),
            library,
        };
        let executables = [
            executable("core", "core", true),
            executable("core-it", "core", false),
            executable("app", "app", true),
        ];
        let sources = |executable: &TestExecutable| {
            let files: &[&str] = match executable.path.file_name()?.to_str()? {
                "core" => &["/ws/core/src/lib.rs"],
                "core-it" => &["/ws/core/tests/it.rs", "/ws/core/tests/common.rs"],
                _ => &["/ws/app/src/lib.rs"],
            };
            Some(files.iter().map(PathBuf::from).collect())
        };
        let skipped = |changed: &[&str]| {
            let changed = changed.iter().map(PathBuf::from).collect();
            unaffected(&executables, &members(), &changed, sources).map(|skip| {
                skip.iter()
                    .map(|path| path.file_name().unwrap().to_string_lossy().into_owned())
                    .collect::<Vec<_>>()
            })
        };

        assert_eq!(skipped(&["/ws/core/tests/common.rs"]).unwrap(), ["core", "app"]);
        assert_eq!(skipped(&["/ws/app/src/lib.rs"]).unwrap(), ["core", "core-it"]);
        // A library's sources affect its package and the packages built with it.
        assert!(skipped(&["/ws/core/src/lib.rs"]).unwrap().is_empty());
        // So does a file no dep-info lists, and the package's manifest.
        assert!(skipped(&["/ws/core/testdata/x"]).unwrap().is_empty());
        assert!(skipped(&["/ws/core/Cargo.toml"]).unwrap().is_empty());
        assert_eq!(skipped(&["/ws/app/testdata/x"]).unwrap(), ["core", "core-it"]);
        assert_eq!(skipped(&[]).unwrap(), ["core", "core-it", "app"]);
        // Workspace config and paths outside every member affect everything.
        assert_eq!(skipped(&["/ws/Cargo.lock"]), None);
        assert_eq!(skipped(&["/elsewhere/x.rs"]), None);
    }

    #[test]
    fn executables_without_dep_info_always_run() {
        let executables = [TestExecutable {
            path: "/ws/target/core-it".into(),
            package: "/ws/core".into(),
            library: false,
        }];
        let changed = ["/ws/app/src/lib.rs".into()].into();
        assert_eq!(unaffected(&executables, &members(), &changed, |_| None), Some(vec![]));
    }

    /// Runs only the executables a change affects: `app`'s test fails, so a
    /// run passes exactly when it skips `app`.
    #[test]
    fn apply_skips_unaffected_executables() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let package = |name: &str, deps: &str| {
            format!("[package]\nname = \"{name}\"\nversion = \"0.1.0\"\nedition = \"2021\"\n[dependencies]\n{deps}")
        };
        for (path, content) in [
            (
                "Cargo.toml",
                "[workspace]\nmembers = [\"core\", \"app\"]\nresolver = \"2\"\n".to_owned(),
            ),
            ("core/Cargo.toml", package("core", "")),
            ("core/src/lib.rs", "pub fn one() -> u32 { 1 }\n".to_owned()),
            (
                "core/tests/it.rs",
                "#[test]\nfn one() { assert_eq!(core::one(), 1); }\n".to_owned(),
            ),
            ("app/Cargo.toml", package("app", "core = { path = \"../core\" }\n")),
            (
                "app/src/lib.rs",
                "#[test]\nfn fails() { assert_eq!(core::one(), 2); }\n".to_owned(),
            ),
        ] {
            let path = root.join(path);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, content).unwrap();
        }
        let status = std::process::Command::new("cargo")
            .args(["generate-lockfile", "--offline"])
            .current_dir(&root)
            .stderr(std::process::Stdio::null())
            .status()
            .unwrap();
        assert!(status.success());

        let inputs = Target::new(
            Some(root.to_string_lossy().into_owned()),
            RustTestInputs {
                package: None,
                flags: vec!["--workspace".into(), "--offline".into()],
                report: ReportMode::default(),
                features: RustFeatures::default(),
                rust_env: RustEnv::default(),
            },
        );
        let out = Output::silent();
        let writer = out.writer("test");
        let apply = |prior: Option<&RustTestState>, changes: &Changes| {
            Resource::apply(&make_resource(), &inputs, prior, changes, &writer).unwrap()
        };
        let changed = |path: &str| Changes::Paths([root.join(path)].into());

        let first = apply(None, &Changes::All);
        assert!(!first.outputs.passed);
        let state = first.state.unwrap();
        assert_eq!(state.executables.len(), 3, "{:?}", state.executables);

        let integration = apply(Some(&state), &changed("core/tests/it.rs"));
        assert!(integration.outputs.passed);
        assert_eq!(integration.state.unwrap().executables.len(), 3);
        assert!(!apply(Some(&state), &changed("core/src/lib.rs")).outputs.passed);
        assert!(!apply(Some(&state), &changed("app/src/lib.rs")).outputs.passed);
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
    fn parse_test_ignored_with_a_reason() {
        for line in [
            "test my_test ... ignored, needs network",
            "test my_test ... ignored (no go)",
        ] {
            match parse_test_line(line) {
                TestEvent::TestIgnored { name } => assert_eq!(name, "my_test"),
                other => panic!("expected TestIgnored for {line:?}, got {other:?}"),
            }
        }
    }

    #[test]
    fn parse_test_trims_libtest_mimic_name_padding() {
        match parse_test_line("test short      ... ok") {
            TestEvent::TestPassed { name } => assert_eq!(name, "short"),
            other => panic!("expected TestPassed, got {other:?}"),
        }
        match parse_test_line("test a ... b      ... FAILED") {
            TestEvent::TestFailed { name } => assert_eq!(name, "a ... b"),
            other => panic!("expected TestFailed, got {other:?}"),
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
        let result = process_test_output(output.lines().map(String::from), &writer, ReportMode::Suites);
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
        let result = process_test_output(output.lines().map(String::from), &writer, ReportMode::Suites);
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
        let result = process_test_output(output.lines().map(String::from), &writer, ReportMode::Suites);
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
    fn parse_failure_output_reads_libtest_mimic_headers() {
        let lines: Vec<String> = vec![
            "---- java_library ----".into(),
            "> bit".into(),
            "FAIL: stdout has no match for 'core ✔ ok'".into(),
        ];
        let failures = parse_failure_output(&lines);
        assert_eq!(failures.len(), 1);
        assert_eq!(failures[0].0, "java_library");
        assert_eq!(failures[0].1, "> bit\nFAIL: stdout has no match for 'core ✔ ok'");
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
        let result = process_test_output(std::iter::empty::<String>(), &writer, ReportMode::Suites);
        assert!(result.passed);
        assert!(result.diagnostics.is_empty());
    }

    #[test]
    fn process_output_retains_compiler_diagnostics_after_passing_suite() {
        let lines = [
            "Running unittests src/lib.rs (target/debug/deps/example)",
            "test result: ok. 1 passed; 0 failed; 0 ignored; finished in 0.1s",
            "   Compiling broken-crate v0.1.0",
            "error[E0283]: type annotations needed",
            " --> src/lib.rs:318:13",
            "error: could not compile `broken-crate` due to 1 previous error",
        ]
        .map(String::from);
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

    /// What a test writes to the stdout handle itself, rather than through
    /// `print!`, escapes libtest's capture, as does a child process's output.
    #[test]
    fn process_output_retains_uncaptured_stdout() {
        let lines = [
            "running 1 test",
            "lib │ Broken.java:1: error: reached end of file while parsing",
            "",
            "test compiles ... FAILED",
            "test result: FAILED. 0 passed; 1 failed; 0 ignored; finished in 0.1s",
        ]
        .map(String::from);
        let out = Output::silent();
        let writer = out.writer("test");
        let result = process_test_output(lines, &writer, ReportMode::Suites);
        assert!(!result.passed);
        assert_eq!(
            result.diagnostics,
            ["lib │ Broken.java:1: error: reached end of file while parsing"]
        );
    }
}
