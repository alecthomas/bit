use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde::{Deserialize, Serialize};

use crate::cache::{ArtifactRef, Cas};
use crate::file_tracker::FileTracker;
use crate::output::TargetWriter;
use crate::provider::{
    Ambient, ApplyResult, BoxError, CachePolicy, Changes, MaterializeResult, OutputFile, PlanAction, PlanResult,
    Resource, ResourceKind, Target, restore_output_files,
};
use crate::sha256::SHA256;

use super::jar::Jar;
use super::maven::MavenDirs;

const DEFAULT_SOURCES: &str = "src/main/java";
const DEFAULT_RESOURCES: &str = "src/main/resources";

/// Compile Java sources with javac into a jar. Every `.java` file beneath `sources` is compiled, and every file beneath `resources` is copied into the jar. The jar is reproducible: equal inputs give an identical file.
#[derive(Debug, Deserialize, bit_derive::Schema)]
pub struct JavaLibraryInputs {
    /// Source directories, relative to `dir` (defaults to `src/main/java`)
    #[serde(default, deserialize_with = "crate::providers::string_or_vec")]
    #[schema(typ = crate::schema::optional_string_or_list())]
    pub sources: Vec<String>,
    /// Resource directories, relative to `dir` (defaults to `src/main/resources`)
    #[serde(default, deserialize_with = "crate::providers::string_or_vec")]
    #[schema(typ = crate::schema::optional_string_or_list())]
    pub resources: Vec<String>,
    /// Jars and class directories to compile against, relative to `dir`, such as another library's `classpath` output. Each is tracked as an input.
    #[serde(default)]
    pub classpath: Vec<String>,
    /// Maven artifacts to compile against, as `group:artifact` keys in `lock`, each with everything it depends on
    #[serde(default)]
    pub maven: Vec<String>,
    /// Annotation processors to run, as `group:artifact` keys in `lock`. Only these run, not processors found on the class path.
    #[serde(default)]
    pub processors: Vec<String>,
    /// Lockfile that `maven` and `processors` come from, relative to `dir`, such as a java.maven target's `lock` output
    #[serde(default)]
    pub lock: Option<String>,
    /// Java release to compile for, passed as `--release`
    #[serde(default)]
    pub release: Option<u32>,
    /// Extra flags passed to javac
    #[serde(default)]
    pub flags: Vec<String>,
    /// Output jar path, relative to `dir` (defaults to `build/<name>.jar`, where `<name>` is the last element of `dir`)
    #[serde(default)]
    pub output: Option<String>,
    /// Extra input file globs, relative to `dir` (in addition to sources, resources and the class path)
    #[serde(default)]
    pub inputs: Vec<String>,
}

/// Outputs from a `java.library` target.
#[derive(Debug, Serialize, bit_derive::Schema)]
pub struct JavaLibraryOutputs {
    /// Absolute path to the built jar
    pub jar: String,
    /// Absolute paths of the jar followed by the class path and Maven jars it was compiled against: what code using this library compiles and runs with
    pub classpath: Vec<String>,
}

/// Persisted state for a `java.library` target.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct JavaLibraryState {
    pub sources: Vec<String>,
    pub resources: Vec<String>,
    pub classpath: Vec<String>,
    #[serde(default)]
    pub maven: Vec<String>,
    #[serde(default)]
    pub processors: Vec<String>,
    #[serde(default)]
    pub lock: Option<String>,
    pub release: Option<u32>,
    pub flags: Vec<String>,
    /// The jar, relative to the target's `dir`.
    pub output: String,
}

pub struct JavaLibraryResource {
    tracker: Arc<FileTracker>,
    maven: MavenDirs,
}

impl JavaLibraryResource {
    pub(super) fn new(tracker: Arc<FileTracker>, maven: MavenDirs) -> Self {
        Self { tracker, maven }
    }

    fn maven_jars(
        &self,
        inputs: &Target<JavaLibraryInputs>,
        artifacts: &[String],
    ) -> Result<Vec<(PathBuf, SHA256)>, BoxError> {
        super::maven_jars(inputs, inputs.lock.as_deref(), artifacts, &self.maven)
    }

    fn outputs_for(&self, inputs: &Target<JavaLibraryInputs>) -> Result<JavaLibraryOutputs, BoxError> {
        let jar = super::absolute(inputs, &output_name(inputs)?)?
            .to_string_lossy()
            .into_owned();
        let maven = self
            .maven_jars(inputs, &inputs.maven)?
            .into_iter()
            .map(|(path, _)| path);
        let mut entries = vec![jar.clone()];
        for entry in classpath(inputs)?.into_iter().chain(maven) {
            let entry = entry.to_string_lossy().into_owned();
            if !entries.contains(&entry) {
                entries.push(entry);
            }
        }
        Ok(JavaLibraryOutputs {
            jar,
            classpath: entries,
        })
    }
}

/// The jar's path relative to `dir`: `output`, or `build/<name>.jar`.
fn output_name(inputs: &Target<JavaLibraryInputs>) -> Result<String, BoxError> {
    if let Some(output) = &inputs.output {
        return Ok(output.clone());
    }
    Ok(format!("build/{}.jar", super::dir_name(inputs)?))
}

/// `dirs`, or `default` when empty, as absolute paths.
fn dirs_or(inputs: &Target<JavaLibraryInputs>, dirs: &[String], default: &str) -> Result<Vec<PathBuf>, BoxError> {
    if dirs.is_empty() {
        return Ok(vec![super::absolute(inputs, default)?]);
    }
    dirs.iter().map(|dir| super::absolute(inputs, dir)).collect()
}

fn source_dirs(inputs: &Target<JavaLibraryInputs>) -> Result<Vec<PathBuf>, BoxError> {
    dirs_or(inputs, &inputs.sources, DEFAULT_SOURCES)
}

fn resource_dirs(inputs: &Target<JavaLibraryInputs>) -> Result<Vec<PathBuf>, BoxError> {
    dirs_or(inputs, &inputs.resources, DEFAULT_RESOURCES)
}

fn classpath(inputs: &Target<JavaLibraryInputs>) -> Result<Vec<PathBuf>, BoxError> {
    inputs
        .classpath
        .iter()
        .map(|entry| super::absolute(inputs, entry))
        .collect()
}

/// The arguments to javac other than the class path, output directory and
/// sources: the target's options.
fn option_args(inputs: &JavaLibraryInputs) -> Vec<String> {
    super::javac_options(inputs.release, &inputs.flags)
}

fn describe(inputs: &Target<JavaLibraryInputs>) -> Result<String, BoxError> {
    let sources = if inputs.sources.is_empty() {
        vec![DEFAULT_SOURCES.to_owned()]
    } else {
        inputs.sources.clone()
    };
    let mut args = option_args(inputs);
    args.extend(sources);
    let command = crate::providers::display_command("javac", &args);
    Ok(format!("{command} → {}", inputs.path(&output_name(inputs)?)))
}

fn state_for(inputs: &Target<JavaLibraryInputs>) -> Result<JavaLibraryState, BoxError> {
    Ok(JavaLibraryState {
        sources: inputs.sources.clone(),
        resources: inputs.resources.clone(),
        classpath: inputs.classpath.clone(),
        maven: inputs.maven.clone(),
        processors: inputs.processors.clone(),
        lock: inputs.lock.clone(),
        release: inputs.release,
        flags: inputs.flags.clone(),
        output: output_name(inputs)?,
    })
}

/// Explicitly named directories must exist. The defaults are conventions,
/// so a library without resources simply has none.
fn check_dirs(inputs: &Target<JavaLibraryInputs>) -> Result<(), BoxError> {
    if (!inputs.maven.is_empty() || !inputs.processors.is_empty()) && inputs.lock.is_none() {
        return Err("set `lock` to the lockfile `maven` and `processors` come from".into());
    }
    for (field, dirs) in [("sources", &inputs.sources), ("resources", &inputs.resources)] {
        for dir in dirs.iter() {
            if !Path::new(&inputs.path(dir)).is_dir() {
                return Err(format!("{field} directory `{dir}` does not exist").into());
            }
        }
    }
    Ok(())
}

impl Resource for JavaLibraryResource {
    type State = JavaLibraryState;
    type Inputs = JavaLibraryInputs;
    type Outputs = JavaLibraryOutputs;

    fn name(&self) -> &str {
        "library"
    }

    fn kind(&self) -> ResourceKind {
        ResourceKind::Build
    }

    fn ambient(&self, inputs: &Target<JavaLibraryInputs>) -> Vec<Ambient> {
        super::ambient(&["javac"], inputs)
    }

    fn resolve(&self, inputs: &Target<JavaLibraryInputs>) -> Result<BTreeMap<String, SHA256>, BoxError> {
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
        let maven = [&inputs.maven, &inputs.processors];
        for (path, sha256) in maven
            .iter()
            .map(|keys| self.maven_jars(inputs, keys))
            .collect::<Result<Vec<_>, _>>()?
            .into_iter()
            .flatten()
        {
            files.insert(path.to_string_lossy().into_owned(), sha256);
        }
        for glob in &inputs.inputs {
            files.extend(self.tracker.hash_glob(&inputs.glob(glob))?);
        }
        Ok(files)
    }

    fn plan(
        &self,
        inputs: &Target<JavaLibraryInputs>,
        prior_state: Option<&JavaLibraryState>,
    ) -> Result<PlanResult, BoxError> {
        check_dirs(inputs)?;
        let description = describe(inputs)?;
        let action = match prior_state {
            None => PlanAction::Create,
            Some(prior) if *prior != state_for(inputs)? => PlanAction::Update,
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
        inputs: &Target<JavaLibraryInputs>,
        _prior_state: Option<&JavaLibraryState>,
        _changes: &Changes,
        writer: &TargetWriter,
    ) -> Result<ApplyResult<JavaLibraryState, JavaLibraryOutputs>, BoxError> {
        let sources = super::files_in(&source_dirs(inputs)?, Some("java"))?;
        let resource_dirs: Vec<PathBuf> = resource_dirs(inputs)?.into_iter().filter(|dir| dir.is_dir()).collect();
        if sources.is_empty() && super::files_in(&resource_dirs, None)?.is_empty() {
            return Err("no Java sources or resources to build".into());
        }

        let mut jar = Jar::default();
        // Compiled into a fresh directory, so classes of deleted sources
        // can't reach the jar.
        let work = tempfile::tempdir()?;
        // Fetched even with nothing to compile, as the `classpath` output
        // names them.
        let mut classpath = classpath(inputs)?;
        classpath.extend(super::fetch_maven_jars(
            inputs,
            inputs.lock.as_deref(),
            &inputs.maven,
            &self.maven,
        )?);
        if !sources.is_empty() {
            let classes = work.path().join("classes");
            let processors = super::fetch_maven_jars(inputs, inputs.lock.as_deref(), &inputs.processors, &self.maven)?;
            let compile = super::Compile {
                sources: &sources,
                classpath: &classpath,
                processors: &processors,
                options: &option_args(inputs),
            };
            super::compile(&compile, &classes, inputs, writer)?;
            jar.add_dir(&classes)?;
        }
        for dir in &resource_dirs {
            jar.add_dir(dir)?;
        }
        jar.write(Path::new(&inputs.path(&output_name(inputs)?)))?;

        Ok(ApplyResult {
            outputs: self.outputs_for(inputs)?,
            state: Some(state_for(inputs)?),
        })
    }

    fn destroy(&self, prior_state: &Target<JavaLibraryState>, writer: &TargetWriter) -> Result<(), BoxError> {
        crate::providers::remove_path(Path::new(&prior_state.path(&prior_state.output)), writer)
    }

    fn cache_policy(&self, _inputs: &Target<JavaLibraryInputs>) -> CachePolicy {
        CachePolicy::Shared { version: 1 }
    }

    fn output_keys(&self, inputs: &Target<JavaLibraryInputs>) -> Vec<String> {
        output_name(inputs)
            .map(|output| vec![inputs.path(&output)])
            .unwrap_or_default()
    }

    /// The outputs are absolute paths in the worktree that built them, so
    /// they are rebuilt from the current inputs. The Maven jars the
    /// `classpath` output names are fetched, as another machine may have
    /// built the jar.
    fn materialize(
        &self,
        inputs: &Target<JavaLibraryInputs>,
        _state: &JavaLibraryState,
        outputs: &[OutputFile],
        artifacts: &BTreeMap<String, ArtifactRef>,
        cas: &Cas,
        writer: &TargetWriter,
    ) -> MaterializeResult<JavaLibraryState, JavaLibraryOutputs> {
        restore_output_files(outputs, artifacts, cas, writer)?;
        super::fetch_maven_jars(inputs, inputs.lock.as_deref(), &inputs.maven, &self.maven)?;
        Ok(Some(ApplyResult {
            outputs: self.outputs_for(inputs)?,
            state: Some(state_for(inputs)?),
        }))
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::io::Read;

    use super::*;
    use crate::output::Output;

    fn resource() -> JavaLibraryResource {
        JavaLibraryResource::new(Arc::new(FileTracker::default()), MavenDirs::new())
    }

    fn library(dir: &Path) -> Target<JavaLibraryInputs> {
        Target::new(
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
        )
    }

    fn write(dir: &Path, name: &str, content: &str) {
        let path = dir.join(name);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, content).unwrap();
    }

    fn apply(inputs: &Target<JavaLibraryInputs>) -> Result<JavaLibraryOutputs, BoxError> {
        let output = Output::new(&[]);
        let result = Resource::apply(&resource(), inputs, None, &Changes::All, &output.writer("lib"))?;
        Ok(result.outputs)
    }

    fn entry(jar: &str, name: &str) -> Option<Vec<u8>> {
        let mut archive = zip::ZipArchive::new(fs::File::open(jar).unwrap()).unwrap();
        let mut file = archive.by_name(name).ok()?;
        let mut content = Vec::new();
        file.read_to_end(&mut content).unwrap();
        Some(content)
    }

    #[test]
    fn output_defaults_to_the_dir_name_under_build() {
        let dir = tempfile::tempdir().unwrap();
        let core = dir.path().join("core");
        fs::create_dir(&core).unwrap();
        assert_eq!(output_name(&library(&core)).unwrap(), "build/core.jar");
        let mut inputs = library(&core);
        inputs.output = Some("out/lib.jar".into());
        assert_eq!(output_name(&inputs).unwrap(), "out/lib.jar");
    }

    #[test]
    fn plan_updates_when_a_field_changes() {
        let dir = tempfile::tempdir().unwrap();
        let inputs = library(dir.path());
        let prior = state_for(&inputs).unwrap();
        let plan = |inputs: &Target<JavaLibraryInputs>, prior| Resource::plan(&resource(), inputs, prior).unwrap();
        assert_eq!(plan(&inputs, None).action, PlanAction::Create);
        assert_eq!(plan(&inputs, Some(&prior)).action, PlanAction::None);
        let mut changed = library(dir.path());
        changed.release = Some(21);
        assert_eq!(plan(&changed, Some(&prior)).action, PlanAction::Update);
        assert!(
            plan(&changed, None)
                .description
                .starts_with("javac --release 21 src/main/java → "),
        );
    }

    #[test]
    fn maven_artifacts_need_a_lockfile() {
        let dir = tempfile::tempdir().unwrap();
        let mut inputs = library(dir.path());
        inputs.maven = vec!["com.google.guava:guava".into()];
        let error = Resource::plan(&resource(), &inputs, None).unwrap_err().to_string();
        assert!(error.contains("set `lock`"), "{error}");
    }

    #[test]
    fn plan_rejects_a_missing_source_directory() {
        let dir = tempfile::tempdir().unwrap();
        let mut inputs = library(dir.path());
        inputs.sources = vec!["src".into()];
        let error = Resource::plan(&resource(), &inputs, None).unwrap_err().to_string();
        assert!(error.contains("sources directory `src` does not exist"), "{error}");
    }

    #[test]
    fn resolve_tracks_sources_resources_and_the_classpath() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        write(root, "src/main/java/com/example/App.java", "class App {}");
        write(root, "src/main/java/com/example/README.md", "");
        write(root, "src/main/resources/app.properties", "");
        write(root, "libs/dep.jar", "jar");
        write(root, "classes/com/example/Dep.class", "class");
        let mut inputs = library(root);
        inputs.classpath = vec!["libs/dep.jar".into(), "classes".into()];

        let files = Resource::resolve(&resource(), &inputs).unwrap();
        let tracked = |name: &str| files.keys().any(|path| path.ends_with(name));
        assert!(tracked("com/example/App.java"), "{files:?}");
        assert!(tracked("app.properties"), "{files:?}");
        assert!(tracked("libs/dep.jar"), "{files:?}");
        assert!(tracked("classes/com/example/Dep.class"), "{files:?}");
        assert!(!tracked("README.md"), "{files:?}");
    }

    #[test]
    fn apply_compiles_against_another_library() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        write(
            &root,
            "core/src/main/java/com/example/core/Greeter.java",
            "package com.example.core; public class Greeter { public static String greet() { return \"hi\"; } }",
        );
        write(&root, "core/src/main/resources/core.properties", "name=core");
        write(
            &root,
            "app/src/main/java/com/example/app/App.java",
            "package com.example.app; public class App { String s = com.example.core.Greeter.greet(); }",
        );

        let core = apply(&library(&root.join("core"))).unwrap();
        assert_eq!(core.jar, root.join("core/build/core.jar").to_string_lossy());
        assert!(entry(&core.jar, "com/example/core/Greeter.class").is_some());
        assert_eq!(entry(&core.jar, "core.properties").unwrap(), b"name=core");

        let mut app_inputs = library(&root.join("app"));
        app_inputs.classpath = core.classpath.clone();
        let app = apply(&app_inputs).unwrap();
        assert!(entry(&app.jar, "com/example/app/App.class").is_some());
        assert!(entry(&app.jar, "com/example/core/Greeter.class").is_none());
        assert_eq!(app.classpath, [app.jar.clone(), core.jar.clone()]);

        // A rebuild from the same sources gives the same jar.
        let before = fs::read(&core.jar).unwrap();
        apply(&library(&root.join("core"))).unwrap();
        assert_eq!(before, fs::read(&core.jar).unwrap());
    }

    #[test]
    fn apply_reports_compile_errors() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "src/main/java/Broken.java", "class Broken {");
        let error = apply(&library(dir.path())).unwrap_err().to_string();
        assert!(error.contains("`javac` exited with"), "{error}");
    }

    #[test]
    fn apply_rejects_an_empty_library() {
        let dir = tempfile::tempdir().unwrap();
        let error = apply(&library(dir.path())).unwrap_err().to_string();
        assert!(error.contains("no Java sources or resources"), "{error}");
    }

    #[test]
    fn destroy_removes_the_jar() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "build/lib.jar", "jar");
        let inputs = library(dir.path());
        let mut state = state_for(&inputs).unwrap();
        state.output = "build/lib.jar".into();
        let output = Output::new(&[]);
        Resource::destroy(
            &resource(),
            &Target::new(inputs.dir.clone(), state),
            &output.writer("lib"),
        )
        .unwrap();
        assert!(!dir.path().join("build/lib.jar").exists());
    }
}
