//! Protobuf providers: `buf` drives the Buf CLI and `protoc` drives the
//! Protobuf compiler directly.

pub mod buf;
mod imports;
pub mod protoc;

use std::collections::BTreeMap;
use std::io::BufReader;
use std::path::{Component, Path, PathBuf};
use std::process::{Command, ExitStatus, Stdio};

use crate::file_tracker::FileTracker;
use crate::output::BlockWriter;
use crate::provider::{Ambient, BoxError, PlanAction, PlanResult};
use crate::sha256::SHA256;

/// Run `program args...` in `dir`, streaming its output through the writer.
fn run(program: &str, args: &[String], dir: Option<&str>, writer: &BlockWriter) -> Result<ExitStatus, BoxError> {
    let mut cmd = Command::new(program);
    cmd.args(args).stdout(Stdio::piped()).stderr(Stdio::piped());
    if let Some(dir) = dir {
        cmd.current_dir(dir);
    }
    let mut child = cmd.spawn().map_err(|e| format!("failed to execute `{program}`: {e}"))?;

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

    Ok(child
        .wait()
        .map_err(|e| format!("failed to wait for `{program}`: {e}"))?)
}

/// Generators compiled into protoc, which run without a `protoc-gen-*`
/// plugin.
const PROTOC_BUILTIN_GENERATORS: [&str; 11] = [
    "cpp", "csharp", "java", "kotlin", "objc", "php", "pyi", "python", "rbs", "ruby", "rust",
];

/// A plugin executable run from `dir`, fingerprinted by content because
/// plugins have no reliable version output: many lack a version flag, and
/// some wait on stdin for a request. A path names a file relative to
/// `dir`, and a bare name is searched for on `PATH`.
fn plugin(program: &str, dir: Option<&str>) -> Ambient {
    if program.contains('/') {
        Ambient::Executable(in_dir(dir, program).to_string_lossy().into_owned())
    } else {
        Ambient::Executable(program.to_owned())
    }
}

/// `program --version`, run in `dir` as the resource runs it, which also
/// resolves a relative `program` against `dir`.
fn version(program: &str, dir: Option<&str>) -> Ambient {
    crate::providers::version(program, &["--version"], dir)
}

/// Plan a block whose configuration was `prior` when it last ran.
fn plan<T: PartialEq>(description: String, prior: Option<&T>, current: &T) -> PlanResult {
    let action = match prior {
        None => PlanAction::Create,
        Some(prior) if prior != current => PlanAction::Update,
        Some(_) => PlanAction::None,
    };
    PlanResult {
        action,
        description,
        reason: None,
    }
}

/// `path` relative to the block's working directory, as bit's process sees it.
fn in_dir(dir: Option<&str>, path: &str) -> PathBuf {
    Path::new(dir.unwrap_or("")).join(path)
}

/// `.proto` files under `root`. Hidden directories such as `.git` are skipped.
fn proto_files(root: &Path) -> Result<Vec<PathBuf>, BoxError> {
    // An empty root is the current directory; joining it with `/` would
    // search from the filesystem root instead.
    let root = root.to_string_lossy();
    let root = root.trim_end_matches('/');
    let pattern = if root.is_empty() {
        "**/*.proto".to_owned()
    } else {
        format!("{}/**/*.proto", glob::Pattern::escape(root))
    };
    let options = glob::MatchOptions {
        require_literal_leading_dot: true,
        ..Default::default()
    };
    let mut files = Vec::new();
    for path in glob::glob_with(&pattern, options)? {
        let path = path?;
        if path.is_file() {
            files.push(path);
        }
    }
    files.sort();
    Ok(files)
}

/// Hash the `.proto` files under each of `roots`.
fn hash_protos(roots: &[PathBuf], tracker: &FileTracker) -> Result<BTreeMap<String, SHA256>, BoxError> {
    let mut hashes = BTreeMap::new();
    for root in roots {
        hashes.extend(tracker.hash_files(&proto_files(root)?)?);
    }
    Ok(hashes)
}

/// The generated output directories that bit may delete on destroy.
///
/// Generators are often pointed back into the source tree (`out: .` with
/// `paths=source_relative`), and output paths are derived from configuration
/// rather than listed by the user, so a directory that holds any `.proto`
/// file is treated as source and never claimed.
fn removable_outputs(outputs: &[PathBuf]) -> Result<Vec<String>, BoxError> {
    let mut removable = Vec::new();
    for output in outputs {
        let output = normalize(output);
        if output.is_dir() && !proto_files(&output)?.is_empty() {
            continue;
        }
        let output = output.to_string_lossy().into_owned();
        if !removable.contains(&output) {
            removable.push(output);
        }
    }
    Ok(removable)
}

/// Drop `.` components and trailing separators so equal paths compare equal.
fn normalize(path: &Path) -> PathBuf {
    let normalized: PathBuf = path.components().filter(|c| *c != Component::CurDir).collect();
    if normalized.as_os_str().is_empty() {
        PathBuf::from(".")
    } else {
        normalized
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn proto_files_skips_hidden_directories() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::create_dir_all(root.join("acme/v1")).unwrap();
        std::fs::create_dir_all(root.join(".cache")).unwrap();
        std::fs::write(root.join("acme/v1/a.proto"), "").unwrap();
        std::fs::write(root.join("acme/v1/a.go"), "").unwrap();
        std::fs::write(root.join(".cache/b.proto"), "").unwrap();
        assert_eq!(proto_files(root).unwrap(), vec![root.join("acme/v1/a.proto")]);
    }

    #[test]
    fn removable_outputs_keeps_directories_holding_sources() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::create_dir_all(root.join("proto")).unwrap();
        std::fs::create_dir_all(root.join("gen")).unwrap();
        std::fs::write(root.join("proto/a.proto"), "").unwrap();
        std::fs::write(root.join("gen/a.pb.go"), "").unwrap();
        let outputs = [
            root.join("gen"),
            root.join("./gen/"),
            root.join("proto"),
            root.to_path_buf(),
            root.join("missing"),
        ];
        assert_eq!(
            removable_outputs(&outputs).unwrap(),
            vec![
                root.join("gen").to_string_lossy().into_owned(),
                root.join("missing").to_string_lossy().into_owned(),
            ]
        );
    }

    #[test]
    fn plan_compares_configuration() {
        assert_eq!(plan("x".into(), None, &1).action, PlanAction::Create);
        assert_eq!(plan("x".into(), Some(&1), &1).action, PlanAction::None);
        assert_eq!(plan("x".into(), Some(&1), &2).action, PlanAction::Update);
    }
}
