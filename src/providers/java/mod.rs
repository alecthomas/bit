//! Java built directly with the JDK's tools, without Gradle or Maven.

pub mod binary;
mod classfile;
mod jar;
mod junit;
pub mod library;
pub mod maven;
pub mod test;

use crate::process::CommandExt;
use std::collections::BTreeMap;
use std::io::BufReader;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;

use crate::file_tracker::FileTracker;
use crate::output::TargetWriter;
use crate::provider::{Ambient, BoxError, DynResource, FuncSignature, Provider, Target};
use crate::sha256::SHA256;
use crate::value::Value;

/// Java provider with `library`, `binary`, `test` and `maven` resources.
pub struct JavaProvider {
    tracker: Arc<FileTracker>,
    maven: maven::MavenDirs,
}

impl JavaProvider {
    pub fn new(tracker: Arc<FileTracker>) -> Self {
        Self {
            tracker,
            maven: maven::MavenDirs::new(),
        }
    }
}

impl Provider for JavaProvider {
    fn name(&self) -> &str {
        "java"
    }

    fn resources(&self) -> Vec<Box<dyn DynResource>> {
        vec![
            Box::new(library::JavaLibraryResource::new(
                self.tracker.clone(),
                self.maven.clone(),
            )),
            Box::new(binary::JavaBinaryResource::new(self.tracker.clone())),
            Box::new(test::JavaTestResource::new(self.tracker.clone(), self.maven.clone())),
            Box::new(maven::JavaMavenResource::new(self.tracker.clone(), self.maven.clone())),
        ]
    }

    fn functions(&self) -> Vec<FuncSignature> {
        vec![]
    }

    fn call_function(&self, name: &str, _args: &[Value]) -> Result<Value, BoxError> {
        Err(format!("java provider has no function '{name}'").into())
    }
}

/// The versions of the JDK tools in `programs`, run in the target's `dir`,
/// where a version manager such as jenv or asdf selects the JDK, plus the
/// variables every JVM reads its options from, which change how the tools
/// behave. `--version` rather than `-version`, which prints to stderr.
fn ambient<T>(programs: &[&str], target: &Target<T>) -> Vec<Ambient> {
    let mut ambient: Vec<Ambient> = programs
        .iter()
        .map(|program| crate::providers::version(program, &["--version"], target.dir.as_deref()))
        .collect();
    ambient.push(Ambient::Variable("JAVA_TOOL_OPTIONS".into()));
    ambient.push(Ambient::Variable("_JAVA_OPTIONS".into()));
    ambient
}

/// `path`, relative to the target's `dir`, as an absolute path. Paths are
/// passed between targets absolute, because each resolves relative paths
/// against its own `dir`. The engine rewrites them into a portable form for
/// the shared cache key.
fn absolute<T>(target: &Target<T>, path: &str) -> Result<PathBuf, BoxError> {
    Ok(std::env::current_dir()?.join(target.path(path)))
}

/// The last element of the target's `dir`, which names its default output.
fn dir_name<T>(target: &Target<T>) -> Result<String, BoxError> {
    let dir = absolute(target, ".")?;
    let dir = dir.canonicalize().unwrap_or(dir);
    Ok(dir
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| "java".to_owned()))
}

/// Every file beneath each directory in `dirs` whose name ends in
/// `extension`, or every file when it is `None`. A directory that does not
/// exist contributes nothing.
fn files_in(dirs: &[PathBuf], extension: Option<&str>) -> Result<Vec<PathBuf>, BoxError> {
    let mut files = Vec::new();
    for dir in dirs.iter().filter(|dir| dir.is_dir()) {
        files.extend(
            crate::providers::files_under(dir)?
                .into_iter()
                .filter(|file| extension.is_none_or(|extension| file.extension().is_some_and(|e| e == extension))),
        );
    }
    Ok(files)
}

/// Hash every class path entry: a jar's content, or each file in a class
/// directory.
fn hash_classpath(classpath: &[PathBuf], tracker: &FileTracker) -> Result<BTreeMap<String, SHA256>, BoxError> {
    let mut files = Vec::new();
    for entry in classpath {
        if entry.is_dir() {
            files.extend(crate::providers::files_under(entry)?);
        } else {
            files.push(entry.clone());
        }
    }
    tracker.hash_files(&files)
}

/// `entries` joined into a class path argument.
fn join_classpath(entries: &[PathBuf]) -> Result<String, BoxError> {
    Ok(std::env::join_paths(entries)?.to_string_lossy().into_owned())
}

/// The javac options a target sets.
fn javac_options(release: Option<u32>, flags: &[String]) -> Vec<String> {
    let mut args = Vec::new();
    if let Some(release) = release {
        args.extend(["--release".to_owned(), release.to_string()]);
    }
    args.extend(flags.iter().cloned());
    args
}

/// What javac compiles and against what.
struct Compile<'a> {
    sources: &'a [PathBuf],
    classpath: &'a [PathBuf],
    /// Annotation processors to run.
    processors: &'a [PathBuf],
    /// [`javac_options`].
    options: &'a [String],
}

/// Compile into `classes` with javac.
fn compile<T>(compile: &Compile, classes: &Path, target: &Target<T>, writer: &TargetWriter) -> Result<(), BoxError> {
    let work = tempfile::tempdir()?;
    let argfile = work.path().join("sources");
    write_argfile(&argfile, compile.sources)?;
    let mut cmd = Command::new("javac");
    cmd.arg("-d").arg(classes);
    if !compile.classpath.is_empty() {
        cmd.arg("-classpath").arg(join_classpath(compile.classpath)?);
    }
    // Only the processors named run: those found on the class path would
    // otherwise also run on JDKs before 23, which changed the default,
    // making the output depend on the JDK. Generated sources are compiled
    // with the rest and not kept.
    if compile.processors.is_empty() {
        cmd.arg("-proc:none");
    } else {
        cmd.arg("-processorpath").arg(join_classpath(compile.processors)?);
        cmd.arg("-s").arg(work.path().join("generated"));
        std::fs::create_dir_all(work.path().join("generated"))?;
    }
    // Sources found on the class path are compiled only as far as checking
    // them, so no class file of theirs is written.
    cmd.arg("-implicit:none");
    cmd.args(compile.options);
    cmd.arg(format!("@{}", argfile.display()));
    run(cmd, target, writer)
}

/// The jars the lockfile at `lock`, relative to the target's `dir`, names
/// for `artifacts`, each a `group:artifact` key, with everything they
/// depend on. Each is its path in bit's Maven cache and the hash the
/// lockfile records for it, which [`fetch_maven_jars`] checks.
fn maven_jars<T>(
    target: &Target<T>,
    lock: Option<&str>,
    artifacts: &[String],
    dirs: &maven::MavenDirs,
) -> Result<Vec<(PathBuf, SHA256)>, BoxError> {
    if artifacts.is_empty() {
        return Ok(Vec::new());
    }
    let lock =
        lock.ok_or("set `lock` to the lockfile Maven artifacts come from, such as a java.maven target's `lock`")?;
    let keys = artifacts
        .iter()
        .map(|artifact| artifact.parse::<maven::Key>())
        .collect::<Result<Vec<_>, _>>()?;
    maven::jars(&absolute(target, lock)?, &keys, dirs)
}

/// [`maven_jars`], fetched into the cache unless they are already there.
fn fetch_maven_jars<T>(
    target: &Target<T>,
    lock: Option<&str>,
    artifacts: &[String],
    dirs: &maven::MavenDirs,
) -> Result<Vec<PathBuf>, BoxError> {
    if artifacts.is_empty() {
        return Ok(Vec::new());
    }
    let lock = absolute(target, lock.unwrap_or_default())?;
    let keys = artifacts
        .iter()
        .map(|artifact| artifact.parse::<maven::Key>())
        .collect::<Result<Vec<_>, _>>()?;
    maven::fetch_jars(&lock, &keys, dirs)
}

/// Write a javac argument file naming `files`, so that a large source tree
/// doesn't exceed the command line length limit. Each path is quoted, with
/// `\` and `"` escaped, as javac's argument file syntax requires.
fn write_argfile(path: &Path, files: &[PathBuf]) -> Result<(), BoxError> {
    let mut content = String::new();
    for file in files {
        let file = file.to_string_lossy().replace('\\', "\\\\").replace('"', "\\\"");
        content.push_str(&format!("\"{file}\"\n"));
    }
    std::fs::write(path, content)?;
    Ok(())
}

/// Run `cmd` in the target's `dir` with its `env`, streaming its output
/// through `writer`. Returns `Err` on a non-zero exit.
fn run<T>(mut cmd: Command, target: &Target<T>, writer: &TargetWriter) -> Result<(), BoxError> {
    let program = cmd.get_program().to_string_lossy().into_owned();
    if let Some(dir) = &target.dir {
        cmd.current_dir(dir);
    }
    cmd.envs(&target.env).stdout(Stdio::piped()).stderr(Stdio::piped());
    let mut child = cmd
        .logged_spawn()
        .map_err(|e| format!("failed to execute `{program}`: {e}"))?;
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
    let status = child
        .wait()
        .map_err(|e| format!("failed to wait for `{program}`: {e}"))?;
    if !status.success() {
        return Err(format!("`{program}` exited with {status}").into());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn argfile_quotes_each_path() {
        let dir = tempfile::tempdir().unwrap();
        let argfile = dir.path().join("sources");
        write_argfile(
            &argfile,
            &[PathBuf::from("/src/A.java"), PathBuf::from(r#"/my "src"\B.java"#)],
        )
        .unwrap();
        assert_eq!(
            std::fs::read_to_string(argfile).unwrap(),
            "\"/src/A.java\"\n\"/my \\\"src\\\"\\\\B.java\"\n"
        );
    }

    #[test]
    fn files_in_filters_by_extension_and_skips_missing_dirs() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("com/example")).unwrap();
        std::fs::write(dir.path().join("com/example/A.java"), "").unwrap();
        std::fs::write(dir.path().join("com/example/notes.txt"), "").unwrap();
        let dirs = [dir.path().to_path_buf(), dir.path().join("missing")];
        assert_eq!(
            files_in(&dirs, Some("java")).unwrap(),
            [dir.path().join("com/example/A.java")]
        );
        assert_eq!(files_in(&dirs, None).unwrap().len(), 2);
    }
}
