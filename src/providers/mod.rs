use std::collections::HashMap;
use std::path::Path;
use std::process::Command;
use std::sync::{LazyLock, Mutex};

use crate::provider::BoxError;
use crate::sha256::SHA256;

pub mod docker;
pub mod exec;
pub mod go;
pub mod pnpm;
pub mod rust;

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
