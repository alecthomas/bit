use std::collections::BTreeMap;
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde::{Deserialize, Serialize};

use crate::cache::{ArtifactRef, Cas};
use crate::file_tracker::FileTracker;
use crate::output::TargetWriter;
use crate::provider::{
    ApplyResult, BoxError, CachePolicy, Changes, MaterializeResult, OutputFile, PlanAction, PlanResult, Resource,
    ResourceKind, Target, restore_output_files,
};
use crate::sha256::SHA256;

use super::classfile::{self, Main};
use super::jar::Jar;

/// Package a runnable Java application into a directory: `<name>.jar`, whose manifest names the main class and the jars copied into `lib/`. Run it with `java -jar`, and copy the whole directory to deploy it.
#[derive(Debug, Deserialize, bit_derive::Schema)]
pub struct JavaBinaryInputs {
    /// Jars the application runs with, relative to `dir`, such as a library's `classpath` output
    pub classpath: Vec<String>,
    /// Main class (defaults to the only class in the first `classpath` jar with a `main` method; one declared `public static void main(String[])` is preferred over JDK 25's instance and parameterless forms)
    #[serde(default)]
    pub main: Option<String>,
    /// Output directory, relative to `dir` (defaults to `build/<name>`, where `<name>` is the last element of `dir`). The jar takes the directory's name.
    #[serde(default)]
    pub output: Option<String>,
}

/// Outputs from a `java.binary` target.
#[derive(Debug, Serialize, bit_derive::Schema)]
pub struct JavaBinaryOutputs {
    /// Absolute path to the runnable jar
    pub jar: String,
    /// Absolute path to the directory holding the jar and its `lib/`
    pub dir: String,
}

/// Persisted state for a `java.binary` target.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct JavaBinaryState {
    pub classpath: Vec<String>,
    pub main: Option<String>,
    /// The output directory, relative to the target's `dir`.
    pub output: String,
}

pub struct JavaBinaryResource {
    tracker: Arc<FileTracker>,
}

impl JavaBinaryResource {
    pub fn new(tracker: Arc<FileTracker>) -> Self {
        Self { tracker }
    }
}

fn output_name(inputs: &Target<JavaBinaryInputs>) -> Result<String, BoxError> {
    if let Some(output) = &inputs.output {
        return Ok(output.trim_end_matches('/').to_owned());
    }
    Ok(format!("build/{}", super::dir_name(inputs)?))
}

/// The output directory, absolute, and the runnable jar in it.
fn output_paths(inputs: &Target<JavaBinaryInputs>) -> Result<(PathBuf, PathBuf), BoxError> {
    let dir = super::absolute(inputs, &output_name(inputs)?)?;
    let name = dir
        .file_name()
        .ok_or_else(|| {
            format!(
                "output `{}` names no directory",
                output_name(inputs).unwrap_or_default()
            )
        })?
        .to_string_lossy()
        .into_owned();
    let jar = dir.join(format!("{name}.jar"));
    Ok((dir, jar))
}

/// The class path entries, absolute, each once.
fn classpath(inputs: &Target<JavaBinaryInputs>) -> Result<Vec<PathBuf>, BoxError> {
    let mut entries = Vec::new();
    for entry in &inputs.classpath {
        let entry = super::absolute(inputs, entry)?;
        if !entries.contains(&entry) {
            entries.push(entry);
        }
    }
    Ok(entries)
}

/// Names for each jar in `lib/`: its own file name, with a counter added
/// when two jars share one.
fn lib_names(jars: &[PathBuf]) -> Vec<String> {
    let mut names: Vec<String> = Vec::new();
    for jar in jars {
        let stem = jar.file_stem().unwrap_or_default().to_string_lossy();
        let extension = jar
            .extension()
            .map(|e| format!(".{}", e.to_string_lossy()))
            .unwrap_or_default();
        let mut name = format!("{stem}{extension}");
        let mut n = 2;
        while names.contains(&name) {
            name = format!("{stem}-{n}{extension}");
            n += 1;
        }
        names.push(name);
    }
    names
}

/// `name` as a relative URL, which is what a manifest `Class-Path` holds.
fn url_path(name: &str) -> String {
    let mut out = String::new();
    for byte in name.bytes() {
        if byte.is_ascii_alphanumeric() || b"-._~+/".contains(&byte) {
            out.push(char::from(byte));
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }
    out
}

/// The class to launch from `jar`, as [`JavaBinaryInputs::main`] describes.
fn find_main(jar: &Path) -> Result<String, BoxError> {
    let mut archive = zip::ZipArchive::new(fs::File::open(jar)?)
        .map_err(|e| format!("cannot read {} as a jar: {e}", jar.display()))?;
    let mut found: BTreeMap<Main, Vec<String>> = BTreeMap::new();
    for i in 0..archive.len() {
        let mut entry = archive.by_index(i)?;
        let name = entry.name()?.into_owned();
        // Multi-release variants repeat classes found at the top level.
        if !name.ends_with(".class") || name.starts_with("META-INF/") || name.ends_with("module-info.class") {
            continue;
        }
        let mut bytes = Vec::new();
        entry.read_to_end(&mut bytes)?;
        let (class, main) = classfile::main_method(&bytes).map_err(|e| format!("{name} in {}: {e}", jar.display()))?;
        if let Some(main) = main {
            found.entry(main).or_default().push(class);
        }
    }
    match found.into_iter().next() {
        None => Err(format!("no class in {} has a main method: set `main`", jar.display()).into()),
        Some((_, classes)) if classes.len() == 1 => Ok(classes.into_iter().next().unwrap_or_default()),
        Some((_, classes)) => Err(format!(
            "several classes in {} have a main method, so set `main` to one of {}",
            jar.display(),
            classes.join(", ")
        )
        .into()),
    }
}

fn state_for(inputs: &Target<JavaBinaryInputs>) -> Result<JavaBinaryState, BoxError> {
    Ok(JavaBinaryState {
        classpath: inputs.classpath.clone(),
        main: inputs.main.clone(),
        output: output_name(inputs)?,
    })
}

fn outputs_for(inputs: &Target<JavaBinaryInputs>) -> Result<JavaBinaryOutputs, BoxError> {
    let (dir, jar) = output_paths(inputs)?;
    Ok(JavaBinaryOutputs {
        jar: jar.to_string_lossy().into_owned(),
        dir: dir.to_string_lossy().into_owned(),
    })
}

impl Resource for JavaBinaryResource {
    type State = JavaBinaryState;
    type Inputs = JavaBinaryInputs;
    type Outputs = JavaBinaryOutputs;

    fn name(&self) -> &str {
        "binary"
    }

    fn kind(&self) -> ResourceKind {
        ResourceKind::Build
    }

    fn resolve(&self, inputs: &Target<JavaBinaryInputs>) -> Result<BTreeMap<String, SHA256>, BoxError> {
        super::hash_classpath(&classpath(inputs)?, &self.tracker)
    }

    fn plan(
        &self,
        inputs: &Target<JavaBinaryInputs>,
        prior_state: Option<&JavaBinaryState>,
    ) -> Result<PlanResult, BoxError> {
        if inputs.classpath.is_empty() {
            return Err("`classpath` must name at least the jar holding the main class".into());
        }
        let main = inputs.main.as_deref().unwrap_or("main class");
        let description = format!("package {main} → {}", inputs.path(&output_name(inputs)?));
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
        inputs: &Target<JavaBinaryInputs>,
        _prior_state: Option<&JavaBinaryState>,
        _changes: &Changes,
        _writer: &TargetWriter,
    ) -> Result<ApplyResult<JavaBinaryState, JavaBinaryOutputs>, BoxError> {
        let jars = classpath(inputs)?;
        if let Some(entry) = jars.iter().find(|entry| !entry.is_file()) {
            return Err(format!("class path entry {} is not a jar", entry.display()).into());
        }
        let main = match &inputs.main {
            Some(main) => main.clone(),
            None => find_main(jars.first().ok_or("`classpath` is empty")?)?,
        };

        let (dir, launcher) = output_paths(inputs)?;
        // Replaced whole, so no jar the application no longer uses is left
        // on its class path.
        if dir.exists() {
            fs::remove_dir_all(&dir)?;
        }
        let lib = dir.join("lib");
        fs::create_dir_all(&lib)?;
        let names = lib_names(&jars);
        for (jar, name) in jars.iter().zip(&names) {
            fs::copy(jar, lib.join(name))?;
        }
        let mut jar = Jar::default();
        jar.main_class(&main);
        jar.class_path(names.iter().map(|name| url_path(&format!("lib/{name}"))).collect());
        jar.write(&launcher)?;

        Ok(ApplyResult {
            outputs: outputs_for(inputs)?,
            state: Some(state_for(inputs)?),
        })
    }

    fn destroy(&self, prior_state: &Target<JavaBinaryState>, writer: &TargetWriter) -> Result<(), BoxError> {
        crate::providers::remove_path(Path::new(&prior_state.path(&prior_state.output)), writer)
    }

    fn cache_policy(&self, _inputs: &Target<JavaBinaryInputs>) -> CachePolicy {
        CachePolicy::Shared { version: 1 }
    }

    fn output_keys(&self, inputs: &Target<JavaBinaryInputs>) -> Vec<String> {
        output_name(inputs)
            .map(|output| vec![inputs.path(&output)])
            .unwrap_or_default()
    }

    /// The outputs are absolute paths in the worktree that built them, so
    /// they are rebuilt from the current inputs.
    fn materialize(
        &self,
        inputs: &Target<JavaBinaryInputs>,
        _state: &JavaBinaryState,
        outputs: &[OutputFile],
        artifacts: &BTreeMap<String, ArtifactRef>,
        cas: &Cas,
        writer: &TargetWriter,
    ) -> MaterializeResult<JavaBinaryState, JavaBinaryOutputs> {
        restore_output_files(outputs, artifacts, cas, writer)?;
        Ok(Some(ApplyResult {
            outputs: outputs_for(inputs)?,
            state: Some(state_for(inputs)?),
        }))
    }
}

#[cfg(test)]
mod tests {
    use std::process::Command;

    use super::*;
    use crate::output::Output;
    use crate::providers::java::library::{JavaLibraryInputs, JavaLibraryResource};
    use crate::providers::java::maven::MavenDirs;

    fn write(dir: &Path, name: &str, content: &str) {
        let path = dir.join(name);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, content).unwrap();
    }

    /// Build a library from `sources` in `dir` and return its class path.
    fn library(dir: &Path, sources: &[(&str, &str)], classpath: Vec<String>) -> Vec<String> {
        for (name, source) in sources {
            write(dir, &format!("src/main/java/{name}"), source);
        }
        let inputs = Target::new(
            Some(dir.to_string_lossy().into_owned()),
            JavaLibraryInputs {
                sources: vec![],
                resources: vec![],
                classpath,
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
        Resource::apply(&resource, &inputs, None, &Changes::All, &output.writer("lib"))
            .unwrap()
            .outputs
            .classpath
    }

    fn binary(dir: &Path, classpath: Vec<String>, main: Option<&str>) -> Target<JavaBinaryInputs> {
        Target::new(
            Some(dir.to_string_lossy().into_owned()),
            JavaBinaryInputs {
                classpath,
                main: main.map(str::to_owned),
                output: None,
            },
        )
    }

    fn apply(inputs: &Target<JavaBinaryInputs>) -> Result<JavaBinaryOutputs, BoxError> {
        let output = Output::new(&[]);
        let resource = JavaBinaryResource::new(Arc::new(FileTracker::default()));
        Ok(Resource::apply(&resource, inputs, None, &Changes::All, &output.writer("bin"))?.outputs)
    }

    #[test]
    fn packages_a_runnable_application() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let core = library(
            &root.join("core"),
            &[(
                "core/Greeter.java",
                "package core; public class Greeter { public static String greet() { return \"hello\"; } }",
            )],
            vec![],
        );
        let app = library(
            &root.join("app"),
            &[(
                "app/Main.java",
                "package app; public class Main { public static void main(String[] a) { System.out.println(core.Greeter.greet()); } }",
            )],
            core,
        );

        let outputs = apply(&binary(&root.join("app"), app, None)).unwrap();
        assert_eq!(outputs.jar, root.join("app/build/app/app.jar").to_string_lossy());
        assert!(root.join("app/build/app/lib/app.jar").is_file());
        assert!(root.join("app/build/app/lib/core.jar").is_file());

        let run = Command::new("java").arg("-jar").arg(&outputs.jar).output().unwrap();
        assert!(run.status.success(), "{}", String::from_utf8_lossy(&run.stderr));
        assert_eq!(String::from_utf8_lossy(&run.stdout).trim(), "hello");
    }

    #[test]
    fn several_main_classes_need_main_set() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let classpath = library(
            &root,
            &[
                ("A.java", "public class A { public static void main(String[] a) {} }"),
                ("B.java", "public class B { public static void main(String[] a) {} }"),
                ("C.java", "class C { void main() {} }"),
            ],
            vec![],
        );
        let error = apply(&binary(&root, classpath.clone(), None)).unwrap_err().to_string();
        assert!(error.contains("set `main` to one of A, B"), "{error}");

        let outputs = apply(&binary(&root, classpath, Some("B"))).unwrap();
        let run = Command::new("java").arg("-jar").arg(&outputs.jar).output().unwrap();
        assert!(run.status.success(), "{}", String::from_utf8_lossy(&run.stderr));
    }

    #[test]
    fn a_rebuild_drops_jars_no_longer_used() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let classpath = library(&root, &[("Main.java", "class Main { void main() {} }")], vec![]);
        write(&root, "extra/old.jar", "");
        let mut with_extra = classpath.clone();
        with_extra.push(root.join("extra/old.jar").to_string_lossy().into_owned());
        let first = apply(&binary(&root, with_extra, None)).unwrap();
        assert!(Path::new(&first.dir).join("lib/old.jar").is_file());

        let outputs = apply(&binary(&root, classpath, None)).unwrap();
        assert!(!Path::new(&outputs.dir).join("lib/old.jar").exists());
    }

    #[test]
    fn lib_names_disambiguate_jars_with_one_name() {
        let jars = [
            PathBuf::from("/a/build/api.jar"),
            PathBuf::from("/b/build/api.jar"),
            PathBuf::from("/c/api-2.jar"),
            PathBuf::from("/d/api.jar"),
        ];
        assert_eq!(lib_names(&jars), ["api.jar", "api-2.jar", "api-2-2.jar", "api-3.jar"]);
    }

    #[test]
    fn class_path_entries_are_url_encoded() {
        assert_eq!(url_path("lib/my lib%.jar"), "lib/my%20lib%25.jar");
    }

    #[test]
    fn plan_requires_a_classpath() {
        let resource = JavaBinaryResource::new(Arc::new(FileTracker::default()));
        let error = Resource::plan(&resource, &binary(Path::new("."), vec![], None), None)
            .unwrap_err()
            .to_string();
        assert!(error.contains("`classpath` must name"), "{error}");
    }
}
