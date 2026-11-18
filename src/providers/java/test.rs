use crate::process::CommandExt;
use std::collections::BTreeMap;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};

use serde::{Deserialize, Serialize};

use crate::file_tracker::FileTracker;
use crate::output::TargetWriter;
use crate::provider::{
    Ambient, ApplyResult, BoxError, CachePolicy, Changes, PlanAction, PlanResult, Resource, ResourceKind, Target,
};
use crate::providers::ReportMode;
use crate::sha256::SHA256;

use super::maven::MavenDirs;

const DEFAULT_SOURCES: &str = "src/test/java";
const DEFAULT_RESOURCES: &str = "src/test/resources";
const LAUNCHER: &str = "org.junit.platform.console.ConsoleLauncher";

/// Compile tests with javac and run them with the JUnit Platform console launcher. A lockfile from java.maven provides the launcher when it holds a JUnit Platform engine; otherwise `classpath` must, with `junit-platform-console-standalone`, or `junit-platform-console` and an engine such as `junit-jupiter-engine`. Every test class compiled from `sources` runs, with `resources` on the class path, and results are read from the launcher's XML reports.
#[derive(Debug, Deserialize, bit_derive::Schema)]
pub struct JavaTestInputs {
    /// Test source directories, relative to `dir` (defaults to `src/test/java`)
    #[serde(default, deserialize_with = "crate::providers::string_or_vec")]
    #[schema(typ = crate::schema::optional_string_or_list())]
    pub sources: Vec<String>,
    /// Test resource directories, relative to `dir` (defaults to `src/test/resources`)
    #[serde(default, deserialize_with = "crate::providers::string_or_vec")]
    #[schema(typ = crate::schema::optional_string_or_list())]
    pub resources: Vec<String>,
    /// Jars and class directories the tests compile and run with, relative to `dir`, such as the tested library's `classpath` output. Each is tracked as an input.
    #[serde(default)]
    pub classpath: Vec<String>,
    /// Maven artifacts the tests compile and run with, such as `org.junit.jupiter:junit-jupiter`, as `group:artifact` keys in `lock`, each with everything it depends on
    #[serde(default)]
    pub maven: Vec<String>,
    /// Lockfile that `maven` comes from, relative to `dir`, such as a java.maven target's `lock` output
    #[serde(default)]
    pub lock: Option<String>,
    /// Java release to compile for, passed as `--release`
    #[serde(default)]
    pub release: Option<u32>,
    /// Extra flags passed to javac
    #[serde(default)]
    pub flags: Vec<String>,
    /// Extra arguments passed to the launcher's `execute` command, such as `--include-tag=fast`
    #[serde(default)]
    pub args: Vec<String>,
    /// What to show: a line per test class (`suites`), a line per test (`tests`), or a line per test plus the launcher's output as it arrives (`verbose`). Outside `verbose`, the launcher's output is shown only when it fails without a failing test to explain it.
    #[serde(default)]
    pub report: ReportMode,
    /// Extra input file globs, relative to `dir` (in addition to test sources, resources and the class path). Add files tests read from elsewhere.
    #[serde(default)]
    pub inputs: Vec<String>,
}

/// Outputs from a `java.test` target.
#[derive(Debug, Serialize, bit_derive::Schema)]
pub struct JavaTestOutputs {
    /// Whether all tests passed
    pub passed: bool,
}

/// Persisted state for a `java.test` target.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct JavaTestState {
    pub sources: Vec<String>,
    pub resources: Vec<String>,
    pub classpath: Vec<String>,
    #[serde(default)]
    pub maven: Vec<String>,
    #[serde(default)]
    pub lock: Option<String>,
    pub release: Option<u32>,
    pub flags: Vec<String>,
    pub args: Vec<String>,
}

pub struct JavaTestResource {
    tracker: Arc<FileTracker>,
    maven: MavenDirs,
}

impl JavaTestResource {
    pub(super) fn new(tracker: Arc<FileTracker>, maven: MavenDirs) -> Self {
        Self { tracker, maven }
    }

    /// The console launcher, when the lockfile holds one: the tests run
    /// with it, but don't compile against it.
    fn launcher(inputs: &Target<JavaTestInputs>) -> Result<Vec<String>, BoxError> {
        let Some(lock) = &inputs.lock else {
            return Ok(Vec::new());
        };
        let console = super::maven::junit_console();
        if super::maven::has(&super::absolute(inputs, lock)?, &console)? {
            Ok(vec![console.to_string()])
        } else {
            Ok(Vec::new())
        }
    }
}

fn dirs_or(inputs: &Target<JavaTestInputs>, dirs: &[String], default: &str) -> Result<Vec<PathBuf>, BoxError> {
    if dirs.is_empty() {
        return Ok(vec![super::absolute(inputs, default)?]);
    }
    dirs.iter().map(|dir| super::absolute(inputs, dir)).collect()
}

fn source_dirs(inputs: &Target<JavaTestInputs>) -> Result<Vec<PathBuf>, BoxError> {
    dirs_or(inputs, &inputs.sources, DEFAULT_SOURCES)
}

fn resource_dirs(inputs: &Target<JavaTestInputs>) -> Result<Vec<PathBuf>, BoxError> {
    dirs_or(inputs, &inputs.resources, DEFAULT_RESOURCES)
}

fn classpath(inputs: &Target<JavaTestInputs>) -> Result<Vec<PathBuf>, BoxError> {
    inputs
        .classpath
        .iter()
        .map(|entry| super::absolute(inputs, entry))
        .collect()
}

fn state_for(inputs: &JavaTestInputs) -> JavaTestState {
    JavaTestState {
        sources: inputs.sources.clone(),
        resources: inputs.resources.clone(),
        classpath: inputs.classpath.clone(),
        maven: inputs.maven.clone(),
        lock: inputs.lock.clone(),
        release: inputs.release,
        flags: inputs.flags.clone(),
        args: inputs.args.clone(),
    }
}

/// The launcher's arguments, scanning only the compiled tests so that
/// test classes inside dependency jars don't run.
fn launcher_args(inputs: &JavaTestInputs, classes: &Path, reports: &Path) -> Vec<String> {
    let details = if inputs.report == ReportMode::Verbose {
        "tree"
    } else {
        "none"
    };
    let mut args = vec![
        LAUNCHER.to_owned(),
        "execute".to_owned(),
        format!("--scan-class-path={}", classes.display()),
        format!("--reports-dir={}", reports.display()),
        "--fail-if-no-tests".to_owned(),
        "--disable-banner".to_owned(),
        "--disable-ansi-colors".to_owned(),
        format!("--details={details}"),
    ];
    args.extend(inputs.args.iter().cloned());
    args
}

/// Whether some class path entry holds the launcher, so that a missing
/// one is reported as such rather than as a failing test run.
fn has_launcher(classpath: &[PathBuf]) -> bool {
    let class = format!("{}.class", LAUNCHER.replace('.', "/"));
    classpath.iter().any(|entry| {
        if entry.is_dir() {
            return entry.join(&class).is_file();
        }
        std::fs::File::open(entry)
            .ok()
            .and_then(|file| zip::ZipArchive::new(file).ok())
            .is_some_and(|mut jar| jar.by_name(&class).is_ok())
    })
}

/// Run the launcher, showing its output as it arrives in verbose mode, and
/// return its exit status with the output it printed.
fn run_launcher(
    cmd: &mut Command,
    verbose: bool,
    writer: &TargetWriter,
) -> Result<(std::process::ExitStatus, Vec<String>), BoxError> {
    let mut child = cmd
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .logged_spawn()
        .map_err(|e| format!("failed to execute `java`: {e}"))?;
    let printed = Mutex::new(Vec::new());
    let collect = |reader: &mut dyn BufRead, stderr: bool| {
        for line in reader.lines().map_while(Result::ok) {
            if verbose {
                if stderr {
                    writer.stderr_line(&line);
                } else {
                    writer.line(&line);
                }
            }
            printed.lock().unwrap_or_else(|e| e.into_inner()).push(line);
        }
    };
    let stdout = child.stdout.take();
    let stderr = child.stderr.take();
    std::thread::scope(|s| {
        if let Some(out) = stdout {
            s.spawn(|| collect(&mut BufReader::new(out), false));
        }
        if let Some(err) = stderr {
            s.spawn(|| collect(&mut BufReader::new(err), true));
        }
    });
    let status = child.wait().map_err(|e| format!("failed to wait for `java`: {e}"))?;
    Ok((status, printed.into_inner().unwrap_or_else(|e| e.into_inner())))
}

impl Resource for JavaTestResource {
    type State = JavaTestState;
    type Inputs = JavaTestInputs;
    type Outputs = JavaTestOutputs;

    fn name(&self) -> &str {
        "test"
    }

    fn kind(&self) -> ResourceKind {
        ResourceKind::Test
    }

    fn ambient(&self, inputs: &Target<JavaTestInputs>) -> Vec<Ambient> {
        super::ambient(&["javac", "java"], inputs)
    }

    fn resolve(&self, inputs: &Target<JavaTestInputs>) -> Result<BTreeMap<String, SHA256>, BoxError> {
        let mut files = self
            .tracker
            .hash_files(&super::files_in(&source_dirs(inputs)?, Some("java"))?)?;
        files.extend(
            self.tracker
                .hash_files(&super::files_in(&resource_dirs(inputs)?, None)?)?,
        );
        files.extend(super::hash_classpath(&classpath(inputs)?, &self.tracker)?);
        // Maven jars are known by the hash the lockfile records, which
        // fetching them checks, so they aren't read here.
        let mut maven = inputs.maven.clone();
        maven.extend(Self::launcher(inputs)?);
        for (path, sha256) in super::maven_jars(inputs, inputs.lock.as_deref(), &maven, &self.maven)? {
            files.insert(path.to_string_lossy().into_owned(), sha256);
        }
        for glob in &inputs.inputs {
            files.extend(self.tracker.hash_glob(&inputs.glob(glob))?);
        }
        Ok(files)
    }

    fn plan(
        &self,
        inputs: &Target<JavaTestInputs>,
        prior_state: Option<&JavaTestState>,
    ) -> Result<PlanResult, BoxError> {
        for (field, dirs) in [("sources", &inputs.sources), ("resources", &inputs.resources)] {
            if let Some(dir) = dirs.iter().find(|dir| !Path::new(&inputs.path(dir)).is_dir()) {
                return Err(format!("{field} directory `{dir}` does not exist").into());
            }
        }
        if !inputs.maven.is_empty() && inputs.lock.is_none() {
            return Err("set `lock` to the lockfile `maven` comes from".into());
        }
        let sources = if inputs.sources.is_empty() {
            vec![DEFAULT_SOURCES.to_owned()]
        } else {
            inputs.sources.clone()
        };
        let mut args = vec!["execute".to_owned()];
        args.extend(inputs.args.iter().cloned());
        args.extend(sources);
        let description = crate::providers::display_command("junit", &args);
        let action = match prior_state {
            None => PlanAction::Create,
            Some(prior) if *prior != state_for(inputs) => PlanAction::Update,
            Some(_) => PlanAction::None,
        };
        Ok(PlanResult {
            action,
            description,
            reason: None,
        })
    }

    fn apply(
        &self,
        inputs: &Target<JavaTestInputs>,
        _prior_state: Option<&JavaTestState>,
        _changes: &Changes,
        writer: &TargetWriter,
    ) -> Result<ApplyResult<JavaTestState, JavaTestOutputs>, BoxError> {
        let sources = super::files_in(&source_dirs(inputs)?, Some("java"))?;
        if sources.is_empty() {
            return Err("no Java test sources to run".into());
        }
        let lock = inputs.lock.as_deref();
        let mut classpath = classpath(inputs)?;
        classpath.extend(super::fetch_maven_jars(inputs, lock, &inputs.maven, &self.maven)?);
        let runtime_jars = super::fetch_maven_jars(inputs, lock, &Self::launcher(inputs)?, &self.maven)?;
        if !has_launcher(&classpath) && !has_launcher(&runtime_jars) {
            return Err(format!(
                "there is no JUnit console launcher ({LAUNCHER}): resolve a JUnit engine such as \
                 org.junit.jupiter:junit-jupiter with java.maven and add it to `maven`, or add the \
                 junit-platform-console-standalone jar to `classpath`"
            )
            .into());
        }

        let work = tempfile::tempdir()?;
        let classes = work.path().join("classes");
        let reports = work.path().join("reports");
        let compile = super::Compile {
            sources: &sources,
            classpath: &classpath,
            processors: &[],
            options: &super::javac_options(inputs.release, &inputs.flags),
        };
        super::compile(&compile, &classes, inputs, writer)?;

        let mut runtime = vec![classes.clone()];
        runtime.extend(resource_dirs(inputs)?.into_iter().filter(|dir| dir.is_dir()));
        runtime.extend(classpath);
        runtime.extend(runtime_jars);
        let mut cmd = Command::new("java");
        cmd.arg("-classpath")
            .arg(super::join_classpath(&runtime)?)
            .args(launcher_args(inputs, &classes, &reports))
            .envs(&inputs.env);
        if let Some(dir) = &inputs.dir {
            cmd.current_dir(dir);
        }
        let verbose = inputs.report == ReportMode::Verbose;
        let (status, printed) = run_launcher(&mut cmd, verbose, writer)?;

        let suites = if reports.is_dir() {
            super::junit::read_reports(&reports)?
        } else {
            Vec::new()
        };
        let tests_passed = super::junit::report(&suites, writer, inputs.report);
        // A failed launch, a failing class initializer, or finding no tests
        // fails the run without a failing test to show for it.
        if !status.success() && tests_passed && !verbose {
            writer.stderr_details(&printed);
        }
        Ok(ApplyResult {
            outputs: JavaTestOutputs {
                passed: status.success() && tests_passed,
            },
            state: Some(state_for(inputs)),
        })
    }

    fn destroy(&self, _prior_state: &Target<JavaTestState>, _writer: &TargetWriter) -> Result<(), BoxError> {
        Ok(())
    }

    fn cache_policy(&self, _inputs: &Target<JavaTestInputs>) -> CachePolicy {
        CachePolicy::Shared { version: 1 }
    }
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::*;
    use crate::output::Output;
    use crate::providers::java::library::{JavaLibraryInputs, JavaLibraryResource};

    /// A stand-in for the JUnit console launcher: it writes a report with
    /// one test per test class it was asked to scan, failing those whose
    /// name contains `Failing`, and exits as JUnit's would. The arguments
    /// it was given become the passing test's output.
    const FAKE_LAUNCHER: &str = r#"
package org.junit.platform.console;

import java.nio.file.*;
import java.util.*;

public class ConsoleLauncher {
    public static void main(String[] args) throws Exception {
        Path classes = null, reports = null;
        for (String arg : args) {
            if (arg.startsWith("--scan-class-path=")) classes = Paths.get(arg.substring(18));
            if (arg.startsWith("--reports-dir=")) reports = Paths.get(arg.substring(14));
        }
        List<String> tests = new ArrayList<>();
        Path root = classes;
        try (var files = Files.walk(root)) {
            files.filter(f -> f.toString().endsWith("Test.class"))
                 .forEach(f -> tests.add(root.relativize(f).toString().replace(".class", "").replace('/', '.')));
        }
        if (tests.isEmpty()) { System.err.println("No tests found"); System.exit(2); }
        StringBuilder xml = new StringBuilder("<testsuite name=\"JUnit Jupiter\">");
        boolean failed = false;
        for (String test : tests) {
            xml.append("<testcase name=\"works()\" classname=\"").append(test).append("\" time=\"0.25\">");
            if (test.contains("Failing")) {
                failed = true;
                xml.append("<failure message=\"nope\">AssertionFailedError: nope</failure>");
            } else {
                xml.append("<system-out>").append(String.join(" ", args)).append("</system-out>");
            }
            xml.append("</testcase>");
        }
        Files.createDirectories(reports);
        Files.writeString(reports.resolve("TEST-junit-jupiter.xml"), xml.append("</testsuite>"));
        System.exit(failed ? 1 : 0);
    }
}
"#;

    fn write(dir: &Path, name: &str, content: &str) {
        let path = dir.join(name);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, content).unwrap();
    }

    /// The fake launcher, built as a jar in `root/launcher`.
    fn launcher_jar(root: &Path) -> String {
        let dir = root.join("launcher");
        write(&dir, "src/main/java/ConsoleLauncher.java", FAKE_LAUNCHER);
        let inputs = Target::new(
            Some(dir.to_string_lossy().into_owned()),
            JavaLibraryInputs {
                sources: vec![],
                resources: vec![],
                classpath: vec![],
                maven: vec![],
                processors: vec![],
                lock: None,
                release: None,
                flags: vec![],
                output: None,
                inputs: vec![],
            },
        );
        let output = Output::new(&[]);
        let resource = JavaLibraryResource::new(Arc::new(FileTracker::default()), MavenDirs::new());
        Resource::apply(&resource, &inputs, None, &Changes::All, &output.writer("launcher"))
            .unwrap()
            .outputs
            .jar
    }

    fn test_target(dir: &Path, classpath: Vec<String>) -> Target<JavaTestInputs> {
        Target::new(
            Some(dir.to_string_lossy().into_owned()),
            JavaTestInputs {
                sources: vec![],
                resources: vec![],
                classpath,
                maven: vec![],
                lock: None,
                release: None,
                flags: vec![],
                args: vec!["--include-tag=fast".into()],
                report: ReportMode::Tests,
                inputs: vec![],
            },
        )
    }

    fn apply(inputs: &Target<JavaTestInputs>) -> Result<bool, BoxError> {
        let output = Output::new(&[]);
        let resource = JavaTestResource::new(Arc::new(FileTracker::default()), MavenDirs::new());
        Ok(
            Resource::apply(&resource, inputs, None, &Changes::All, &output.writer("test"))?
                .outputs
                .passed,
        )
    }

    #[test]
    fn runs_the_compiled_tests_with_the_launcher() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let launcher = launcher_jar(&root);
        write(
            &root,
            "lib/src/test/java/com/example/GreeterTest.java",
            "package com.example; class GreeterTest {}",
        );
        write(&root, "lib/src/test/resources/fixture.txt", "data");
        let inputs = test_target(&root.join("lib"), vec![launcher.clone()]);
        assert!(apply(&inputs).unwrap());

        write(
            &root,
            "lib/src/test/java/com/example/FailingTest.java",
            "package com.example; class FailingTest {}",
        );
        assert!(!apply(&inputs).unwrap());
    }

    #[test]
    fn passes_its_arguments_to_the_launcher() {
        let inputs = test_target(Path::new("."), vec![]);
        let args = launcher_args(&inputs, Path::new("/w/classes"), Path::new("/w/reports"));
        assert_eq!(
            args,
            [
                LAUNCHER,
                "execute",
                "--scan-class-path=/w/classes",
                "--reports-dir=/w/reports",
                "--fail-if-no-tests",
                "--disable-banner",
                "--disable-ansi-colors",
                "--details=none",
                "--include-tag=fast",
            ]
        );
    }

    #[test]
    fn finding_no_tests_fails_the_run() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let launcher = launcher_jar(&root);
        write(&root, "lib/src/test/java/Helper.java", "class Helper {}");
        assert!(!apply(&test_target(&root.join("lib"), vec![launcher])).unwrap());
    }

    #[test]
    fn a_missing_launcher_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "src/test/java/ATest.java", "class ATest {}");
        let error = apply(&test_target(dir.path(), vec![])).unwrap_err().to_string();
        assert!(error.contains("no JUnit console launcher"), "{error}");
    }

    #[test]
    fn resolve_tracks_test_sources_resources_and_the_classpath() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "src/test/java/ATest.java", "class ATest {}");
        write(dir.path(), "src/test/resources/fixture.json", "{}");
        write(dir.path(), "libs/junit.jar", "jar");
        let inputs = test_target(dir.path(), vec!["libs/junit.jar".into()]);
        let resource = JavaTestResource::new(Arc::new(FileTracker::default()), MavenDirs::new());
        let files = Resource::resolve(&resource, &inputs).unwrap();
        for name in ["ATest.java", "fixture.json", "libs/junit.jar"] {
            assert!(files.keys().any(|path| path.ends_with(name)), "{name}: {files:?}");
        }
    }
}
