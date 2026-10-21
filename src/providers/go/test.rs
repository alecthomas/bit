use std::collections::{BTreeMap, HashMap};
use std::io::{BufRead, BufReader};
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::file_tracker::FileTracker;
use crate::output::BlockWriter;
use crate::provider::{
    Ambient, ApplyResult, BoxError, CachePolicy, Changes, PlanAction, PlanResult, Resource, ResourceKind,
};
use crate::sha256::SHA256;

use super::GoEnv;
use super::scanner::Tests;
use crate::providers::ReportMode;

/// Run Go tests
#[derive(Debug, Deserialize, bit_derive::Schema)]
pub struct GoTestInputs {
    /// Go package pattern (e.g. "./...")
    pub package: String,
    /// Extra flags passed to go test
    #[serde(default)]
    pub flags: Vec<String>,
    /// Input file glob patterns (in addition to Go sources and `testdata/`)
    #[serde(default)]
    pub inputs: Vec<String>,
    /// What to show: a line per package (`suites`), a line per test
    /// (`tests`), or a line per test plus go's output as it arrives
    /// (`verbose`). Outside `verbose`, go's output is shown only on failure.
    #[serde(default)]
    pub report: ReportMode,
    /// Working directory for the command
    #[serde(default)]
    pub dir: Option<String>,
    #[serde(flatten)]
    pub env: GoEnv,
}

/// Outputs from a `go.test` block.
#[derive(Debug, Serialize, bit_derive::Schema)]
pub struct GoTestOutputs {
    /// Whether all tests passed
    pub passed: bool,
}

/// Persisted state for a `go.test` block.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GoTestState {
    pub package: String,
    pub flags: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dir: Option<String>,
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
    /// Package-level and build output captured instead of streamed outside
    /// verbose mode, so it can be dumped if the run fails.
    diagnostics: Vec<String>,
}

/// Per-package accumulated results.
#[derive(Default)]
struct PackageResults {
    passed: usize,
    failed: usize,
    skipped: usize,
    /// (test_name, duration, captured output)
    failures: Vec<(String, Option<Duration>, String)>,
    /// Per-test output lines, keyed by test name.
    outputs: HashMap<String, Vec<String>>,
}

/// Parse `go test -json` stdout lines, accumulate per-package, and emit
/// test or package results via the writer as `mode` asks.
fn process_go_test_json(reader: impl BufRead, writer: &BlockWriter, mode: ReportMode) -> TestJsonResult {
    let verbose = mode == ReportMode::Verbose;
    let per_test = mode.per_test();
    let mut all_passed = true;
    let mut had_events = false;
    let mut diagnostics = Vec::new();
    let mut packages: HashMap<String, PackageResults> = HashMap::new();
    let mut emit = |text: &str| {
        for line in text.lines() {
            if verbose {
                writer.stderr_line(line);
            } else {
                diagnostics.push(line.to_owned());
            }
        }
    };

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
                        if verbose {
                            writer.line(trimmed);
                        }
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
                if per_test {
                    let duration = event.elapsed.map(Duration::from_secs_f64);
                    writer.test_passed(&event.package, test_name, duration);
                }
                pkg.outputs.remove(test_name);
            }
            "fail" if event.test.is_some() => {
                let test_name = event.test.as_ref().unwrap();
                let pkg = packages.entry(event.package.clone()).or_default();
                let duration = event.elapsed.map(Duration::from_secs_f64);
                let output = pkg
                    .outputs
                    .remove(test_name)
                    .map(|lines| lines.join("\n"))
                    .unwrap_or_default();
                pkg.failed += 1;
                if per_test {
                    writer.test_failed(&event.package, test_name, duration, &output);
                } else {
                    pkg.failures.push((test_name.clone(), duration, output));
                }
            }
            "skip" if event.test.is_some() => {
                let pkg = packages.entry(event.package.clone()).or_default();
                pkg.skipped += 1;
                if per_test {
                    let test_name = event.test.as_ref().unwrap();
                    writer.test_skipped(&event.package, test_name);
                }
            }
            "pass" if event.test.is_none() => {
                had_events = true;
                let duration = event.elapsed.map(Duration::from_secs_f64);
                let pkg = packages.remove(&event.package).unwrap_or_default();
                let total = pkg.passed + pkg.failed + pkg.skipped;
                if total == 0 {
                    continue;
                }
                if !per_test {
                    writer.test_suite_passed(&event.package, duration, pkg.passed, pkg.skipped);
                }
            }
            "fail" if event.test.is_none() => {
                had_events = true;
                all_passed = false;
                let duration = event.elapsed.map(Duration::from_secs_f64);
                let pkg = packages.remove(&event.package).unwrap_or_default();
                // Per-test lines already show failing tests; a package that
                // fails without one (e.g. a build failure) still needs a line.
                if !per_test || pkg.failed == 0 {
                    writer.test_suite_failed(&event.package, duration, pkg.passed, pkg.failed, &pkg.failures);
                }
            }
            "skip" if event.test.is_none() => {
                had_events = true;
                packages.remove(&event.package);
                if !per_test {
                    writer.test_suite_skipped(&event.package);
                }
            }
            // Package-level output (no Test): the per-package summary line emitted
            // before `fail`/`pass`, e.g. "FAIL\texample/cmd/server [build failed]".
            // Compile-time diagnostics, emitted as JSON on stdout when `-json`
            // is used (stderr is empty in that mode).
            "output" | "build-output" => {
                if let Some(text) = &event.output {
                    emit(text);
                }
            }
            "build-fail" => {
                all_passed = false;
            }
            _ => {}
        }
    }

    TestJsonResult {
        passed: all_passed,
        had_events,
        diagnostics,
    }
}

/// The packages a narrowed run tests, out of the `total` the pattern names.
struct Affected {
    packages: Vec<String>,
    total: usize,
}

/// The packages whose tests depend on a changed path, or `None` to test the
/// whole pattern.
fn affected_packages(inputs: &GoTestInputs, changes: &Changes) -> Result<Option<Affected>, BoxError> {
    let Changes::Paths(changed) = changes else {
        return Ok(None);
    };
    let dir = inputs.dir.as_deref().map(std::path::Path::new);
    let scanned = super::scanner::tested_packages(&inputs.package, dir)?;
    // A path no package depends on is an extra input or a deleted file, whose
    // dependents cannot be found.
    if changed
        .iter()
        .any(|path| !scanned.iter().any(|p| p.files.contains(path)))
    {
        return Ok(None);
    }
    // The scanner ignores build constraints, and `go test` fails on a named
    // package they exclude, so only packages `go` lists may be named.
    let listed = listed_packages(inputs)?;
    if listed
        .iter()
        .any(|path| !scanned.iter().any(|p| &p.import_path == path))
    {
        return Ok(None);
    }
    let packages = scanned
        .into_iter()
        .filter(|p| listed.contains(&p.import_path) && changed.iter().any(|path| p.files.contains(path)))
        .map(|p| p.import_path)
        .collect();
    Ok(Some(Affected {
        packages,
        total: listed.len(),
    }))
}

/// The packages `go test` would test for the block's pattern, with the same
/// environment and build tags.
fn listed_packages(inputs: &GoTestInputs) -> Result<Vec<String>, BoxError> {
    let mut cmd = Command::new("go");
    cmd.args(["list", "-e"])
        .args(tag_flags(&inputs.flags))
        .arg(&inputs.package);
    if let Some(dir) = &inputs.dir {
        cmd.current_dir(dir);
    }
    inputs.env.apply_to(&mut cmd);
    let output = cmd
        .output()
        .map_err(|e| format!("failed to execute `go list {}`: {e}", inputs.package))?;
    if !output.status.success() {
        return Err(format!(
            "`go list {}` failed: {}",
            inputs.package,
            String::from_utf8_lossy(&output.stderr).trim()
        )
        .into());
    }
    Ok(super::package_list(&output.stdout))
}

/// The `-tags` flags among `go test` flags, which `go list` also accepts and
/// which change the packages a pattern matches.
fn tag_flags(flags: &[String]) -> Vec<String> {
    let mut tags = Vec::new();
    let mut flags = flags.iter();
    while let Some(flag) = flags.next() {
        let name = flag.trim_start_matches('-');
        if flag.starts_with('-') && name.starts_with("tags=") {
            tags.push(flag.clone());
        } else if flag.starts_with('-') && name == "tags" {
            tags.push(flag.clone());
            tags.extend(flags.next().cloned());
        }
    }
    tags
}

pub struct GoTestResource {
    tracker: Arc<FileTracker>,
}

impl GoTestResource {
    pub fn new(tracker: Arc<FileTracker>) -> Self {
        Self { tracker }
    }
}

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

    fn ambient(&self, inputs: &GoTestInputs) -> Vec<Ambient> {
        vec![super::toolchain(&inputs.env, inputs.dir.as_deref())]
    }

    fn resolve(&self, inputs: &GoTestInputs) -> Result<BTreeMap<String, SHA256>, BoxError> {
        let tracker = &*self.tracker;
        let dir = inputs.dir.as_deref().map(std::path::Path::new);
        super::resolve_go_inputs(&inputs.package, dir, Tests::SourcesAndTestdata, &inputs.inputs, tracker)
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
        changes: &Changes,
        writer: &BlockWriter,
    ) -> Result<ApplyResult<GoTestState, GoTestOutputs>, BoxError> {
        let state = GoTestState {
            package: inputs.package.clone(),
            flags: inputs.flags.clone(),
            dir: inputs.dir.clone(),
            env: inputs.env.clone(),
        };
        let packages = match affected_packages(inputs, changes)? {
            Some(Affected { packages, total }) => {
                writer.line(&format!("{} of {total} packages affected", packages.len()));
                if packages.is_empty() {
                    return Ok(ApplyResult {
                        outputs: GoTestOutputs { passed: true },
                        state: Some(state),
                    });
                }
                packages
            }
            None => vec![inputs.package.clone()],
        };

        let mut args = vec!["test".to_owned(), "-json".to_owned()];
        args.extend(inputs.flags.iter().cloned());
        args.extend(packages);

        let mut cmd = Command::new("go");
        cmd.args(&args).stdout(Stdio::piped()).stderr(Stdio::piped());
        if let Some(dir) = &inputs.dir {
            cmd.current_dir(dir);
        }
        inputs.env.apply_to(&mut cmd);

        let mut child = cmd.spawn().map_err(|e| format!("failed to execute `go test`: {e}"))?;

        let stdout = child.stdout.take();
        let stderr = child.stderr.take();

        let mode = inputs.report;
        let verbose = mode == ReportMode::Verbose;
        let mut result = TestJsonResult {
            passed: true,
            had_events: false,
            diagnostics: Vec::new(),
        };
        let mut stderr_lines = Vec::new();
        std::thread::scope(|s| {
            let json_handle =
                stdout.map(|out| s.spawn(move || process_go_test_json(BufReader::new(out), writer, mode)));
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
            if let Some(handle) = json_handle {
                result = handle.join().unwrap_or(TestJsonResult {
                    passed: false,
                    had_events: false,
                    diagnostics: Vec::new(),
                });
            }
            if let Some(handle) = stderr_handle {
                stderr_lines = handle.join().unwrap_or_default();
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
        if !passed {
            result.diagnostics.extend(stderr_lines);
            writer.stderr_details(&result.diagnostics);
        }

        Ok(ApplyResult {
            outputs: GoTestOutputs { passed },
            state: Some(state),
        })
    }

    fn destroy(&self, _prior_state: &GoTestState, _writer: &BlockWriter) -> Result<(), BoxError> {
        Ok(())
    }

    fn cache_policy(&self, _inputs: &GoTestInputs) -> CachePolicy {
        CachePolicy::Shared { version: 1 }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::output::Output;

    fn test_resource() -> GoTestResource {
        GoTestResource::new(Arc::new(FileTracker::default()))
    }

    #[test]
    fn resource_kind_is_test() {
        let resource = test_resource();
        assert_eq!(Resource::kind(&resource), ResourceKind::Test);
    }

    #[test]
    fn resolve_tracks_testdata_and_extra_inputs() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::write(root.join("go.mod"), "module example.com/test\n").unwrap();
        std::fs::write(root.join("lib.go"), "package lib\n").unwrap();
        std::fs::create_dir_all(root.join("testdata")).unwrap();
        std::fs::write(root.join("testdata/golden.txt"), "golden\n").unwrap();
        std::fs::write(root.join("schema.json"), "{}\n").unwrap();
        let inputs = GoTestInputs {
            package: "./...".into(),
            flags: vec![],
            inputs: vec![root.join("*.json").to_string_lossy().into_owned()],
            report: ReportMode::default(),
            dir: Some(root.to_string_lossy().into_owned()),
            env: GoEnv::default(),
        };
        let resolved = Resource::resolve(&test_resource(), &inputs).unwrap();
        assert!(
            resolved.keys().any(|p| p.ends_with("testdata/golden.txt")),
            "{resolved:?}"
        );
        assert!(resolved.keys().any(|p| p.ends_with("schema.json")), "{resolved:?}");
    }

    #[test]
    fn affected_packages_follow_changed_paths() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        std::fs::write(root.join("go.mod"), "module example.com/m\n").unwrap();
        std::fs::write(root.join("main.go"), "package main\nimport \"example.com/m/lib\"\n").unwrap();
        std::fs::create_dir_all(root.join("lib")).unwrap();
        std::fs::write(root.join("lib/lib.go"), "package lib\n").unwrap();
        std::fs::write(root.join("lib/lib_test.go"), "package lib\n").unwrap();
        std::fs::create_dir_all(root.join("util")).unwrap();
        std::fs::write(root.join("util/util.go"), "package util\n").unwrap();
        // Build constraints exclude this package everywhere tests run.
        std::fs::create_dir_all(root.join("plan9only")).unwrap();
        std::fs::write(
            root.join("plan9only/main_plan9.go"),
            "package main\nimport \"example.com/m/lib\"\n",
        )
        .unwrap();
        let inputs = GoTestInputs {
            package: "./...".into(),
            flags: vec![],
            inputs: vec![],
            report: ReportMode::default(),
            dir: Some(root.to_string_lossy().into_owned()),
            env: GoEnv::default(),
        };
        let affected = |names: &[&str]| {
            let changes = Changes::Paths(names.iter().map(|name| root.join(name)).collect());
            affected_packages(&inputs, &changes).unwrap().map(|affected| {
                assert_eq!(affected.total, 3);
                let mut packages = affected.packages;
                packages.sort();
                packages
            })
        };

        assert_eq!(
            affected(&["lib/lib.go"]).unwrap(),
            ["example.com/m", "example.com/m/lib"]
        );
        assert_eq!(affected(&["lib/lib_test.go"]).unwrap(), ["example.com/m/lib"]);
        assert_eq!(affected(&["util/util.go"]).unwrap(), ["example.com/m/util"]);
        assert_eq!(affected(&["go.mod"]).unwrap().len(), 3);
        assert_eq!(affected(&["lib/removed.go"]), None);
        assert!(affected_packages(&inputs, &Changes::All).unwrap().is_none());
    }

    #[test]
    fn tag_flags_are_forwarded_to_go_list() {
        let flags: Vec<String> = ["-short", "-tags=a,b", "-run", "TestX", "--tags", "c", "-race"]
            .map(String::from)
            .into();
        assert_eq!(tag_flags(&flags), ["-tags=a,b", "--tags", "c"]);
    }

    #[test]
    fn plan_create_when_no_prior_state() {
        let inputs = GoTestInputs {
            package: "./...".into(),
            flags: vec![],
            inputs: vec![],
            report: ReportMode::default(),
            dir: None,
            env: GoEnv::default(),
        };
        let resource = test_resource();
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
            Value::list(vec![
                Value::Str("-timeout".into()),
                Value::Str("30s".into()),
                Value::Str("-race".into()),
            ]),
        );

        let resource: Box<dyn DynResource> = Box::new(test_resource());
        let result = resource.plan(&inputs, None).unwrap();
        assert_eq!(result.action, PlanAction::Create);
    }

    #[test]
    fn plan_update_when_flags_changed() {
        let inputs = GoTestInputs {
            package: "./...".into(),
            flags: vec!["-v".into()],
            inputs: vec![],
            report: ReportMode::default(),
            dir: None,
            env: GoEnv::default(),
        };
        let prior = GoTestState {
            package: "./...".into(),
            flags: vec![],
            dir: None,
            env: GoEnv::default(),
        };
        let resource = test_resource();
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
        let result = process_go_test_json(std::io::Cursor::new(json), &writer, ReportMode::Suites);
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
        let result = process_go_test_json(std::io::Cursor::new(json), &writer, ReportMode::Suites);
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
        let result = process_go_test_json(std::io::Cursor::new(json), &writer, ReportMode::Suites);
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
        let result = process_go_test_json(std::io::Cursor::new(json), &writer, ReportMode::Suites);
        assert!(!result.passed);
    }

    #[test]
    fn process_json_no_events() {
        let out = Output::new(&[]);
        let writer = out.writer("test");
        let result = process_go_test_json(std::io::Cursor::new(""), &writer, ReportMode::Suites);
        assert!(result.passed);
        assert!(!result.had_events);
    }

    #[test]
    fn process_json_build_failure() {
        // With `-json`, build errors are emitted on stdout (stderr is empty).
        // They appear as `build-output` + `build-fail` events keyed by ImportPath,
        // followed by a per-package `output` summary and a package-level `fail`.
        let json = concat!(
            r##"{"ImportPath":"internal/nettrace","Action":"build-output","Output":"# internal/nettrace\n"}"##,
            "\n",
            r#"{"ImportPath":"internal/nettrace","Action":"build-output","Output":"compile: version mismatch\n"}"#,
            "\n",
            r#"{"ImportPath":"internal/nettrace","Action":"build-fail"}"#,
            "\n",
            r#"{"Action":"output","Package":"example/cmd/server","Output":"FAIL\texample/cmd/server [build failed]\n"}"#,
            "\n",
            r#"{"Action":"fail","Package":"example/cmd/server","Elapsed":0,"FailedBuild":"internal/nettrace"}"#,
            "\n",
        );
        let out = Output::new(&[]);
        let writer = out.writer("test");
        let result = process_go_test_json(std::io::Cursor::new(json), &writer, ReportMode::Suites);
        assert!(!result.passed);
        assert!(result.had_events);
        assert_eq!(
            result.diagnostics,
            vec![
                "# internal/nettrace",
                "compile: version mismatch",
                "FAIL\texample/cmd/server [build failed]",
            ]
        );
    }

    #[test]
    fn process_json_verbose_streams_instead_of_capturing() {
        let json = concat!(
            r#"{"ImportPath":"x","Action":"build-output","Output":"compile: error\n"}"#,
            "\n",
            r#"{"Action":"output","Package":"example.com/pkg","Output":"ok\texample.com/pkg\t0.1s\n"}"#,
            "\n",
        );
        let out = Output::new(&[]);
        let writer = out.writer("test");
        let result = process_go_test_json(std::io::Cursor::new(json), &writer, ReportMode::Verbose);
        assert!(result.diagnostics.is_empty());
    }

    #[test]
    fn process_json_tests_mode_captures_output() {
        let json = concat!(
            r#"{"Action":"output","Package":"example.com/pkg","Output":"ok\texample.com/pkg\t0.1s\n"}"#,
            "\n",
            r#"{"Action":"pass","Package":"example.com/pkg","Test":"TestFoo","Elapsed":0.1}"#,
            "\n",
        );
        let out = Output::new(&[]);
        let writer = out.writer("test");
        let result = process_go_test_json(std::io::Cursor::new(json), &writer, ReportMode::Tests);
        assert!(result.passed);
        assert_eq!(result.diagnostics, ["ok\texample.com/pkg\t0.1s"]);
    }

    #[test]
    fn process_json_build_fail_alone_marks_failure() {
        // A standalone `build-fail` (no package-level `fail` follows) must still
        // mark the run as failed, otherwise the engine would fall back to the
        // exit code only when `had_events` is true.
        let json = concat!(r#"{"ImportPath":"x","Action":"build-fail"}"#, "\n",);
        let out = Output::new(&[]);
        let writer = out.writer("test");
        let result = process_go_test_json(std::io::Cursor::new(json), &writer, ReportMode::Suites);
        assert!(!result.passed);
    }
}
