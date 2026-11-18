//! Java built directly with the JDK's tools, without Gradle or Maven.

mod jar;
pub mod library;

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

/// Java provider with `library` resources.
pub struct JavaProvider {
    tracker: Arc<FileTracker>,
}

impl JavaProvider {
    pub fn new(tracker: Arc<FileTracker>) -> Self {
        Self { tracker }
    }
}

impl Provider for JavaProvider {
    fn name(&self) -> &str {
        "java"
    }

    fn resources(&self) -> Vec<Box<dyn DynResource>> {
        vec![Box::new(library::JavaLibraryResource::new(self.tracker.clone()))]
    }

    fn functions(&self) -> Vec<FuncSignature> {
        vec![]
    }

    fn call_function(&self, name: &str, _args: &[Value]) -> Result<Value, BoxError> {
        Err(format!("java provider has no function '{name}'").into())
    }
}

/// The JDK tool `program`'s version, run in the target's `dir`, where a
/// version manager such as jenv or asdf selects the JDK, plus the variables
/// every JVM reads its options from, which change how the tool behaves.
fn ambient<T>(program: &str, target: &Target<T>) -> Vec<Ambient> {
    vec![
        crate::providers::version(program, &["-version"], target.dir.as_deref()),
        Ambient::Variable("JAVA_TOOL_OPTIONS".into()),
        Ambient::Variable("_JAVA_OPTIONS".into()),
    ]
}

/// `path`, relative to the target's `dir`, as an absolute path. Paths are
/// passed between targets absolute, because each resolves relative paths
/// against its own `dir`. The engine rewrites them into a portable form for
/// the shared cache key.
fn absolute<T>(target: &Target<T>, path: &str) -> Result<PathBuf, BoxError> {
    Ok(std::env::current_dir()?.join(target.path(path)))
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
