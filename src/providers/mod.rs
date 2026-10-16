use std::collections::HashMap;
use std::fs;
use std::io::ErrorKind;
use std::path::Path;
use std::process::Command;
use std::sync::{LazyLock, Mutex};

use crate::output::{BlockWriter, Event};
use crate::provider::BoxError;
use crate::sha256::{Hasher, SHA256};

pub mod docker;
pub mod exec;
pub mod go;
pub mod pnpm;
pub mod rust;

/// How much of a test run to show. Shared by the test resources that report
/// individual tests.
#[derive(Debug, Clone, Copy, Default, PartialEq, serde::Deserialize, bit_derive::Schema)]
#[serde(rename_all = "lowercase")]
pub enum ReportMode {
    /// One line per suite.
    #[default]
    Suites,
    /// One line per test.
    Tests,
    /// One line per test, plus the test command's logs as they arrive.
    Verbose,
}

impl ReportMode {
    /// Whether individual tests get their own lines.
    pub fn per_test(self) -> bool {
        self != Self::Suites
    }
}

/// Remove a provider-owned file, directory, or symlink. Missing paths are
/// already clean; every other filesystem error must reach the engine so it
/// does not discard state after an incomplete destroy.
pub(crate) fn remove_path(path: &Path, writer: &BlockWriter) -> Result<(), BoxError> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(format!("failed to inspect {}: {error}", path.display()).into()),
    };

    if metadata.is_dir() {
        writer.event(Event::Starting, &format!("rm -rf {}", path.display()));
        fs::remove_dir_all(path).map_err(|error| format!("failed to remove {}: {error}", path.display()))?;
    } else {
        writer.event(Event::Starting, &format!("rm {}", path.display()));
        fs::remove_file(path).map_err(|error| format!("failed to remove {}: {error}", path.display()))?;
    }
    Ok(())
}

/// Symlink hops `hash_file` follows before treating the chain as a cycle.
/// Matches Linux's `MAXSYMLINKS`.
const MAX_SYMLINK_HOPS: usize = 40;

/// Compute a SHA256 hash for an input file.
///
/// A regular file hashes to the digest of its contents. A symlink also hashes
/// every target in its chain, so retargeting a link changes the hash even when
/// the final content does not. Hermit relies on this: every tool's stub chains
/// to the same `bin/hermit` script, and only the link targets name the
/// version. Targets are hashed as stored by readlink, not resolved, so the
/// hash is the same across checkouts.
pub fn hash_file(path: &Path) -> Result<SHA256, BoxError> {
    let mut link = path.to_path_buf();
    let mut targets = Vec::new();
    while fs::symlink_metadata(&link)?.is_symlink() {
        if targets.len() == MAX_SYMLINK_HOPS {
            return Err(format!("too many levels of symlinks hashing {}", path.display()).into());
        }
        let target = fs::read_link(&link)?;
        // A relative target is relative to the link's directory; joining an
        // absolute target replaces the path.
        link = link.parent().unwrap_or(Path::new("")).join(&target);
        targets.push(target);
    }
    let contents = fs::read(&link)?;
    if targets.is_empty() {
        return Ok(SHA256::digest(&contents));
    }
    let mut hasher = Hasher::new();
    for target in &targets {
        hasher.update(target.as_os_str().as_encoded_bytes());
        hasher.update(b"\0");
    }
    hasher.update(&contents);
    Ok(hasher.finalize())
}

/// Per-process cache of tool probe output (versions, `go env`, ...) so a run
/// with many blocks pays for each probe once.
static PROBE_CACHE: LazyLock<Mutex<HashMap<String, String>>> = LazyLock::new(|| Mutex::new(HashMap::new()));

/// Run `build()` once per distinct `key` and return its trimmed stdout.
pub fn probe_tool(key: &str, build: impl FnOnce() -> Command) -> Result<String, BoxError> {
    if let Some(cached) = PROBE_CACHE.lock().unwrap_or_else(|e| e.into_inner()).get(key) {
        return Ok(cached.clone());
    }
    let mut cmd = build();
    let program = cmd.get_program().to_string_lossy().into_owned();
    let output = cmd
        .stdin(std::process::Stdio::null())
        .output()
        .map_err(|e| format!("failed to run `{program}`: {e}"))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!("`{program}` exited with {}: {}", output.status, stderr.trim()).into());
    }
    let text = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    PROBE_CACHE
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert(key.to_owned(), text.clone());
    Ok(text)
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::symlink;

    use super::*;
    use crate::output::Output;

    #[test]
    fn remove_path_removes_files_and_directories_and_allows_missing_paths() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("file");
        let child_dir = dir.path().join("dir");
        fs::write(&file, "content").unwrap();
        fs::create_dir(&child_dir).unwrap();
        fs::write(child_dir.join("child"), "content").unwrap();
        let output = Output::new(&[]);
        let writer = output.writer("test");

        remove_path(&file, &writer).unwrap();
        remove_path(&child_dir, &writer).unwrap();
        remove_path(&dir.path().join("missing"), &writer).unwrap();

        assert!(!file.exists());
        assert!(!child_dir.exists());
    }

    #[test]
    fn remove_path_reports_inspection_errors() {
        let output = Output::new(&[]);
        let error = remove_path(Path::new("invalid\0path"), &output.writer("test")).unwrap_err();
        assert!(error.to_string().contains("failed to inspect"));
    }

    /// Lay out a Hermit-style chain: `tsk -> .tsk-<version>.pkg -> hermit`.
    fn hermit_chain(dir: &Path, version: &str) -> std::path::PathBuf {
        let pkg = format!(".tsk-{version}.pkg");
        let _ = fs::remove_file(dir.join("tsk"));
        let _ = fs::remove_file(dir.join(&pkg));
        symlink("hermit", dir.join(&pkg)).unwrap();
        symlink(&pkg, dir.join("tsk")).unwrap();
        dir.join("tsk")
    }

    #[test]
    fn hash_file_of_regular_file_is_content_digest() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("file");
        fs::write(&file, "content").unwrap();
        assert_eq!(hash_file(&file).unwrap(), SHA256::digest(b"content"));
    }

    #[test]
    fn hash_file_changes_when_intermediate_link_target_changes() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("hermit"), "script").unwrap();
        let before = hash_file(&hermit_chain(dir.path(), "0.12.1")).unwrap();
        let after = hash_file(&hermit_chain(dir.path(), "0.17.0")).unwrap();
        assert_ne!(before, after);
        assert_ne!(after, SHA256::digest(b"script"));
    }

    #[test]
    fn hash_file_changes_when_content_behind_chain_changes() {
        let dir = tempfile::tempdir().unwrap();
        let tsk = hermit_chain(dir.path(), "0.17.0");
        fs::write(dir.path().join("hermit"), "v1").unwrap();
        let before = hash_file(&tsk).unwrap();
        fs::write(dir.path().join("hermit"), "v2").unwrap();
        assert_ne!(before, hash_file(&tsk).unwrap());
    }

    #[test]
    fn hash_file_uses_relative_link_text_independent_of_location() {
        let a = tempfile::tempdir().unwrap();
        let b = tempfile::tempdir().unwrap();
        for dir in [a.path(), b.path()] {
            fs::create_dir(dir.join("bin")).unwrap();
            fs::write(dir.join("hermit"), "script").unwrap();
            symlink("../hermit", dir.join("bin/tsk")).unwrap();
        }
        assert_eq!(
            hash_file(&a.path().join("bin/tsk")).unwrap(),
            hash_file(&b.path().join("bin/tsk")).unwrap()
        );
    }

    #[test]
    fn hash_file_rejects_symlink_cycle() {
        let dir = tempfile::tempdir().unwrap();
        symlink("b", dir.path().join("a")).unwrap();
        symlink("a", dir.path().join("b")).unwrap();
        let error = hash_file(&dir.path().join("a")).unwrap_err();
        assert!(error.to_string().contains("too many levels of symlinks"));
    }
}
