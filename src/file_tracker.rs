use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use sha2::{Digest, Sha256};

use crate::dag::Dag;
use crate::provider::ResolvedFile;
use crate::providers::hash_file;
use crate::state::StateStore;
use crate::value::Map;

/// File modification time as nanoseconds since UNIX epoch, stored as a string
/// because the value exceeds JSON's safe integer range (2^53).
pub type FileTimestamp = String;

/// Read a `SystemTime` into our string-of-nanos representation.
fn timestamp_from_system_time(t: SystemTime) -> FileTimestamp {
    let d = t.duration_since(SystemTime::UNIX_EPOCH).unwrap_or_default();
    d.as_nanos().to_string()
}

/// Result of content hash computation, including metadata for fast-path caching.
pub struct ContentHashResult {
    pub hash: String,
    pub input_hash: String,
    pub file_timestamps: HashMap<String, FileTimestamp>,
    pub dep_hashes: HashMap<String, String>,
}

/// Prior state fields relevant to file tracking.
#[derive(Default)]
pub struct PriorFileState {
    pub content_hash: String,
    pub input_hash: String,
    pub file_timestamps: HashMap<String, FileTimestamp>,
    pub dep_hashes: HashMap<String, String>,
}

/// Shared cache for filesystem operations within a single engine run.
///
/// Caches glob expansion, file modification times, and content hashes so
/// that multiple blocks referencing the same files avoid redundant I/O.
pub struct FileTracker {
    hash_cache: HashMap<PathBuf, String>,
    mtime_cache: HashMap<PathBuf, FileTimestamp>,
    glob_cache: HashMap<String, Vec<PathBuf>>,
}

impl FileTracker {
    pub fn new() -> Self {
        Self {
            hash_cache: HashMap::new(),
            mtime_cache: HashMap::new(),
            glob_cache: HashMap::new(),
        }
    }

    /// Return the cached modification time for a file, reading from disk on first access.
    pub fn mtime(&mut self, path: &Path) -> Option<FileTimestamp> {
        if let Some(ts) = self.mtime_cache.get(path) {
            return Some(ts.clone());
        }
        let ts = std::fs::metadata(path)
            .and_then(|m| m.modified())
            .ok()
            .map(timestamp_from_system_time)?;
        self.mtime_cache.insert(path.to_path_buf(), ts.clone());
        Some(ts)
    }

    /// Expand `ResolvedFile` entries into concrete file paths.
    /// `InputGlob` patterns are expanded via filesystem glob, with results cached.
    pub fn expand_resolved(&mut self, entries: &[ResolvedFile]) -> Vec<PathBuf> {
        let mut files = Vec::new();
        for entry in entries {
            match entry {
                ResolvedFile::Input(p) | ResolvedFile::Output(p) => {
                    files.push(p.clone());
                }
                ResolvedFile::InputGlob(pattern) => {
                    let expanded = self.glob_cache.entry(pattern.clone()).or_insert_with(|| {
                        let mut result = Vec::new();
                        if let Ok(paths) = glob::glob(pattern) {
                            for path in paths.flatten() {
                                if path.is_file() {
                                    result.push(path);
                                }
                            }
                        }
                        result
                    });
                    files.extend(expanded.iter().cloned());
                }
            }
        }
        files
    }

    /// Invalidate cached hashes and mtimes for output files that may have changed
    /// after an apply.
    pub fn invalidate_outputs(&mut self, entries: &[ResolvedFile]) {
        for entry in entries {
            if let ResolvedFile::Output(p) = entry {
                self.hash_cache.remove(p);
                self.mtime_cache.remove(p);
            }
        }
    }

    /// Check whether all file timestamps and dep hashes match the stored values.
    /// Returns `true` when the stored content hash can be reused (fast path).
    pub fn timestamps_unchanged(
        &mut self,
        files: &[PathBuf],
        stored_timestamps: &HashMap<String, FileTimestamp>,
        dag: &Dag,
        block_name: &str,
        store: &dyn StateStore,
        stored_dep_hashes: &HashMap<String, String>,
    ) -> bool {
        if files.len() != stored_timestamps.len() {
            return false;
        }
        for file in files {
            let key = file.to_string_lossy();
            let Some(stored) = stored_timestamps.get(key.as_ref()) else {
                return false;
            };
            let Some(current) = self.mtime(file) else {
                return false;
            };
            if current != *stored {
                return false;
            }
        }
        let deps = dag.content_deps(block_name);
        if deps.len() != stored_dep_hashes.len() {
            return false;
        }
        for dep in &deps {
            let Some(stored_hash) = stored_dep_hashes.get(dep) else {
                return false;
            };
            let Ok(Some(parent_state)) = store.load(dep) else {
                return false;
            };
            let parent = unwrap_content_hash(&parent_state);
            if parent != *stored_hash {
                return false;
            }
        }
        true
    }

    /// Compute a combined hash of files and parent block states.
    /// If stored timestamps indicate nothing changed, returns the stored hash (fast path).
    pub fn compute_content_hash(
        &mut self,
        files: &[PathBuf],
        dag: &Dag,
        block_name: &str,
        store: &dyn StateStore,
        input_hash: &str,
        prior: &PriorFileState,
    ) -> ContentHashResult {
        // Fast path: reuse stored hash if nothing changed.
        if !prior.content_hash.is_empty()
            && prior.input_hash == input_hash
            && self.timestamps_unchanged(files, &prior.file_timestamps, dag, block_name, store, &prior.dep_hashes)
        {
            return ContentHashResult {
                hash: prior.content_hash.clone(),
                input_hash: input_hash.to_owned(),
                file_timestamps: prior.file_timestamps.clone(),
                dep_hashes: prior.dep_hashes.clone(),
            };
        }

        // Slow path: full content hash.
        let mut hasher = Sha256::new();
        let mut new_timestamps = HashMap::with_capacity(files.len());

        hasher.update(input_hash.as_bytes());

        let mut sorted = files.to_vec();
        sorted.sort();
        for file in &sorted {
            let hash = self
                .hash_cache
                .entry(file.clone())
                .or_insert_with(|| hash_file(file).unwrap_or_default());
            if !hash.is_empty() {
                hasher.update(file.to_string_lossy().as_bytes());
                hasher.update(hash.as_bytes());
            }
            if let Some(ts) = self.mtime(file) {
                new_timestamps.insert(file.to_string_lossy().into_owned(), ts);
            }
        }

        let mut deps = dag.content_deps(block_name);
        deps.sort();
        let mut new_dep_hashes = HashMap::with_capacity(deps.len());
        for dep in &deps {
            if let Ok(Some(state)) = store.load(dep) {
                let parent_hash = unwrap_content_hash(&state);
                new_dep_hashes.insert(dep.clone(), parent_hash);
                hasher.update(dep.as_bytes());
                hasher.update(state.to_string().as_bytes());
            }
        }

        ContentHashResult {
            hash: format!("{:x}", hasher.finalize()),
            input_hash: input_hash.to_owned(),
            file_timestamps: new_timestamps,
            dep_hashes: new_dep_hashes,
        }
    }

    /// Describe why a block's content hash changed compared to its prior state.
    pub fn change_reason(
        &mut self,
        resolved: &[ResolvedFile],
        prior: &PriorFileState,
        dag: &Dag,
        block_name: &str,
        dirty_deps: &std::collections::HashSet<String>,
        input_hash: &str,
    ) -> Option<String> {
        let cwd = std::env::current_dir().unwrap_or_default();

        let mut dirty: Vec<_> = dag
            .content_deps(block_name)
            .into_iter()
            .filter(|d| dirty_deps.contains(d))
            .collect();
        dirty.dedup();
        if !dirty.is_empty() {
            let quoted: Vec<_> = dirty.iter().map(|d| format!("'{d}'")).collect();
            return Some(quoted.join(", ") + " changed");
        }

        let mut changed_inputs = Vec::new();
        let mut changed_outputs = Vec::new();
        for entry in resolved {
            let (path, is_output) = match entry {
                ResolvedFile::Input(p) => (p, false),
                ResolvedFile::Output(p) => (p, true),
                ResolvedFile::InputGlob(pattern) => {
                    let expanded = self.glob_cache.entry(pattern.clone()).or_insert_with(|| {
                        let mut result = Vec::new();
                        if let Ok(paths) = glob::glob(pattern) {
                            for path in paths.flatten() {
                                if path.is_file() {
                                    result.push(path);
                                }
                            }
                        }
                        result
                    });
                    for p in expanded.clone() {
                        let key = p.to_string_lossy();
                        let stored = prior.file_timestamps.get(key.as_ref());
                        let current = self.mtime(&p);
                        if stored.map(String::as_str) != current.as_deref() {
                            changed_inputs.push(relative_display(&p, &cwd));
                        }
                    }
                    continue;
                }
            };
            let key = path.to_string_lossy();
            let stored = prior.file_timestamps.get(key.as_ref());
            let current = self.mtime(path);
            if stored.map(String::as_str) != current.as_deref() {
                let display = relative_display(path, &cwd);
                if is_output {
                    changed_outputs.push(display);
                } else {
                    changed_inputs.push(display);
                }
            }
        }

        if !changed_inputs.is_empty() {
            let count = changed_inputs.len();
            if count <= 3 {
                let quoted: Vec<_> = changed_inputs.iter().map(|f| format!("'{f}'")).collect();
                return Some(quoted.join(", ") + " changed");
            }
            return Some(format!("{count} files changed"));
        }
        if !changed_outputs.is_empty() {
            let quoted: Vec<_> = changed_outputs.iter().map(|p| format!("'{p}'")).collect();
            return Some(format!("output {} modified externally", quoted.join(", ")));
        }

        if prior.file_timestamps.len() != resolved.len() {
            return Some("file set changed".into());
        }

        if !prior.input_hash.is_empty() && prior.input_hash != input_hash {
            return Some("inputs changed".into());
        }

        None
    }
}

impl Default for FileTracker {
    fn default() -> Self {
        Self::new()
    }
}

pub(crate) fn relative_display(path: &Path, cwd: &Path) -> String {
    path.strip_prefix(cwd).unwrap_or(path).to_string_lossy().into_owned()
}

/// Hash the block's evaluated input fields in canonical (key-sorted) JSON form.
pub fn hash_inputs(inputs: &Map) -> String {
    let canonical = serde_json::to_value(inputs)
        .and_then(|v| serde_json::to_string(&v))
        .unwrap_or_default();
    let mut hasher = Sha256::new();
    hasher.update(canonical.as_bytes());
    format!("{:x}", hasher.finalize())
}

/// Extract just the content_hash from a persisted wrapped state.
fn unwrap_content_hash(stored: &serde_json::Value) -> String {
    stored
        .get("content_hash")
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .to_owned()
}
