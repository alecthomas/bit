use std::collections::HashMap;
use std::fs;
use std::io::ErrorKind;
use std::path::Path;
use std::process::Command;
use std::sync::{LazyLock, Mutex};

use crate::output::{BlockWriter, Event};
use crate::provider::BoxError;
use crate::sha256::SHA256;

pub mod docker;
pub mod exec;
pub mod go;
pub mod pnpm;
pub mod rust;

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

/// Compute a SHA256 content hash for a file.
pub fn hash_file(path: &Path) -> Result<SHA256, BoxError> {
    let contents = std::fs::read(path)?;
    Ok(SHA256::digest(&contents))
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
}
