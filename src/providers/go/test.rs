use crate::process::CommandExt;
use std::collections::{BTreeMap, HashMap};
use std::io::{BufRead, BufReader};
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::file_tracker::FileTracker;
use crate::output::BlockWriter;
use crate::provider::{
    Ambient, ApplyResult, Block, BoxError, CachePolicy, Changes, PlanAction, PlanResult, Resource, ResourceKind,
};
use crate::sha256::SHA256;

use super::GoEnv;
use super::scanner::Tests;
use crate::providers::ReportMode;

/// Run Go tests
#[derive(Debug, Deserialize, bit_derive::Schema)]
pub struct GoTestInputs {
    /// Go package pattern or list of patterns (e.g. "./...")
    #[serde(deserialize_with = "super::package_patterns")]
    #[schema(typ = crate::schema::string_or_list())]
    pub package: Vec<String>,
    /// Extra flags passed to go test
    #[serde(default)]
    pub flags: Vec<String>,
    /// Input file glob patterns, relative to `dir` (in addition to Go sources and `testdata/`)
    #[serde(default)]
    pub inputs: Vec<String>,
    /// What to show: a line per package (`suites`), a line per test
    /// (`tests`), or a line per test plus go's output as it arrives
    /// (`verbose`). Outside `verbose`, go's output is shown only on failure.
    #[serde(default)]
    pub report: ReportMode,
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
    /// A string in state written before `package` took a list.
    #[serde(deserialize_with = "crate::providers::string_or_vec")]
    pub package: Vec<String>,
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
/// test or package results via the writer as `mode` asks. `module` is the
/// tested module's path, which result lines elide.
fn process_go_test_json(
    reader: impl BufRead,
    writer: &BlockWriter,
    mode: ReportMode,
    module: Option<&str>,
) -> TestJsonResult {
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
        let shown = super::display_package(&event.package, module);

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
                    writer.test_passed(&shown, test_name, duration);
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
                    writer.test_failed(&shown, test_name, duration, &output);
                } else {
                    pkg.failures.push((test_name.clone(), duration, output));
                }
            }
            "skip" if event.test.is_some() => {
                let pkg = packages.entry(event.package.clone()).or_default();
                pkg.skipped += 1;
                if per_test {
                    let test_name = event.test.as_ref().unwrap();
                    writer.test_skipped(&shown, test_name);
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
                    writer.test_suite_passed(&shown, duration, pkg.passed, pkg.skipped);
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
                    writer.test_suite_failed(&shown, duration, pkg.passed, pkg.failed, &pkg.failures);
                }
            }
            "skip" if event.test.is_none() => {
                had_events = true;
                packages.remove(&event.package);
                if !per_test {
                    writer.test_suite_skipped(&shown);
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
fn affected_packages(inputs: &Block<GoTestInputs>, changes: &Changes) -> Result<Option<Affected>, BoxError> {
    let tags = tag_flags(&inputs.flags);
    let affected = super::affected_packages(&inputs.package, inputs.dir.as_deref(), &tags, &inputs.env, changes)?;
    // The listing uses the test's own tags and environment, so `go test` on
    // the whole pattern skips excluded packages too.
    Ok(affected.map(|affected| Affected {
        packages: affected.packages.into_iter().map(|p| p.import_path).collect(),
        total: affected.total,
    }))
}

/// The arguments to `go` that test `packages`.
fn test_args(inputs: &GoTestInputs, packages: &[String]) -> Vec<String> {
    let mut args = vec!["test".to_owned(), "-json".to_owned()];
    args.extend(inputs.flags.iter().cloned());
    args.extend(packages.iter().cloned());
    args
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

    fn ambient(&self, inputs: &Block<GoTestInputs>) -> Vec<Ambient> {
        vec![super::toolchain(&inputs.env, inputs.dir.as_deref())]
    }

    fn resolve(&self, inputs: &Block<GoTestInputs>) -> Result<BTreeMap<String, SHA256>, BoxError> {
        let tracker = &*self.tracker;
        let dir = inputs.dir.as_deref().map(std::path::Path::new);
        super::resolve_go_inputs(&inputs.package, dir, Tests::SourcesAndTestdata, &inputs.inputs, tracker)
    }

    fn plan(&self, inputs: &Block<GoTestInputs>, prior_state: Option<&GoTestState>) -> Result<PlanResult, BoxError> {
        let description = super::display_command("go", &test_args(inputs, &inputs.package), inputs.dir.as_deref());

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
        inputs: &Block<GoTestInputs>,
        _prior_state: Option<&GoTestState>,
        changes: &Changes,
        writer: &BlockWriter,
    ) -> Result<ApplyResult<GoTestState, GoTestOutputs>, BoxError> {
        let state = GoTestState {
            package: inputs.package.clone(),
            flags: inputs.flags.clone(),
            env: inputs.env.clone(),
        };
        let affected = affected_packages(inputs, changes)?;
        if let Some(Affected { packages, total }) = &affected
            && packages.is_empty()
        {
            writer.starting(&format!("0 of {total} packages affected"));
            return Ok(ApplyResult {
                outputs: GoTestOutputs { passed: true },
                state: Some(state),
            });
        }
        let packages = affected.as_ref().map_or(&inputs.package, |affected| &affected.packages);
        let args = test_args(inputs, packages);
        writer.starting(&super::display_command("go", &args, inputs.dir.as_deref()));
        if let Some(Affected { packages, total }) = &affected {
            writer.line(&format!("{} of {total} packages affected", packages.len()));
        }

        let mut cmd = Command::new("go");
        cmd.args(&args).stdout(Stdio::piped()).stderr(Stdio::piped());
        if let Some(dir) = &inputs.dir {
            cmd.current_dir(dir);
        }
        inputs.env.apply_to(&mut cmd);

        let mut child = cmd
            .logged_spawn()
            .map_err(|e| format!("failed to execute `go test`: {e}"))?;

        let stdout = child.stdout.take();
        let stderr = child.stderr.take();

        let mode = inputs.report;
        let verbose = mode == ReportMode::Verbose;
        // Only shortens result lines, so full import paths are shown when the
        // module cannot be read.
        let module = super::scanner::module_path(inputs.dir.as_deref().map(std::path::Path::new)).ok();
        let module = module.as_deref();
        let mut result = TestJsonResult {
            passed: true,
            had_events: false,
            diagnostics: Vec::new(),
        };
        let mut stderr_lines = Vec::new();
        std::thread::scope(|s| {
            let json_handle =
                stdout.map(|out| s.spawn(move || process_go_test_json(BufReader::new(out), writer, mode, module)));
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

    fn destroy(&self, _prior_state: &Block<GoTestState>, _writer: &BlockWriter) -> Result<(), BoxError> {
        Ok(())
    }

    /// The packages tested depend on the changes.
    fn announces_start(&self) -> bool {
        true
    }

    fn cache_policy(&self, _inputs: &Block<GoTestInputs>) -> CachePolicy {
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
        let inputs = Block::new(
            Some(root.to_string_lossy().into_owned()),
            GoTestInputs {
                package: vec!["./...".into()],
                flags: vec![],
                inputs: vec![root.join("*.json").to_string_lossy().into_owned()],
                report: ReportMode::default(),
                env: GoEnv::default(),
            },
        );
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
        let inputs = Block::new(
            Some(root.to_string_lossy().into_owned()),
            GoTestInputs {
                package: vec!["./...".into()],
                flags: vec![],
                inputs: vec![],
                report: ReportMode::default(),
                env: GoEnv::default(),
            },
        );
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
        let inputs = Block::new(
            None,
            GoTestInputs {
                package: vec!["./...".into()],
                flags: vec![],
                inputs: vec![],
                report: ReportMode::default(),
                env: GoEnv::default(),
            },
        );
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
    fn package_accepts_a_list() {
        let inputs: GoTestInputs = serde_json::from_str(r#"{"package": ["./a/...", "./b"]}"#).unwrap();
        assert_eq!(inputs.package, ["./a/...", "./b"]);
        let result = Resource::plan(&test_resource(), &Block::new(None, inputs), None).unwrap();
        assert_eq!(result.description, "go test -json ./a/... ./b");

        assert!(serde_json::from_str::<GoTestInputs>(r#"{"package": []}"#).is_err());
    }

    #[test]
    fn state_with_a_single_package_still_loads() {
        let state: GoTestState = serde_json::from_str(r#"{"package": "./...", "flags": []}"#).unwrap();
        assert_eq!(state.package, ["./..."]);
    }

    #[test]
    fn plan_update_when_flags_changed() {
        let inputs = Block::new(
            None,
            GoTestInputs {
                package: vec!["./...".into()],
                flags: vec!["-v".into()],
                inputs: vec![],
                report: ReportMode::default(),
                env: GoEnv::default(),
            },
        );
        let prior = GoTestState {
            package: vec!["./...".into()],
            flags: vec![],
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
        let result = process_go_test_json(std::io::Cursor::new(json), &writer, ReportMode::Suites, None);
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
        let result = process_go_test_json(std::io::Cursor::new(json), &writer, ReportMode::Suites, None);
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
        let result = process_go_test_json(std::io::Cursor::new(json), &writer, ReportMode::Suites, None);
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
        let result = process_go_test_json(std::io::Cursor::new(json), &writer, ReportMode::Suites, None);
        assert!(!result.passed);
    }

    #[test]
    fn process_json_no_events() {
        let out = Output::new(&[]);
        let writer = out.writer("test");
        let result = process_go_test_json(std::io::Cursor::new(""), &writer, ReportMode::Suites, None);
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
        let result = process_go_test_json(std::io::Cursor::new(json), &writer, ReportMode::Suites, None);
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
        let result = process_go_test_json(std::io::Cursor::new(json), &writer, ReportMode::Verbose, None);
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
        let result = process_go_test_json(std::io::Cursor::new(json), &writer, ReportMode::Tests, None);
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
        let result = process_go_test_json(std::io::Cursor::new(json), &writer, ReportMode::Suites, None);
        assert!(!result.passed);
    }
}
