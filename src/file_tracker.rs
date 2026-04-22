use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};

use crate::provider::BoxError;
use crate::providers::hash_file;
use crate::sha256::SHA256;

/// Shared cache for filesystem operations within a single engine run.
///
/// Caches glob expansion and file content hashes so that multiple blocks
/// referencing the same files avoid redundant I/O.
pub struct FileTracker {
    hash_cache: HashMap<PathBuf, SHA256>,
    glob_cache: HashMap<String, Vec<PathBuf>>,
}

impl FileTracker {
    pub fn new() -> Self {
        Self {
            hash_cache: HashMap::new(),
            glob_cache: HashMap::new(),
        }
    }

    /// Compute the SHA256 of a file's contents, returning a cached result
    /// if available.
    pub fn hash_file(&mut self, path: &Path) -> Result<SHA256, BoxError> {
        if let Some(hash) = self.hash_cache.get(path) {
            return Ok(*hash);
        }
        let hash = hash_file(path)?;
        self.hash_cache.insert(path.to_path_buf(), hash);
        Ok(hash)
    }

    /// Expand a glob pattern and hash each matching file.
    /// Returns entries keyed by the file path (relative as matched by glob).
    pub fn hash_glob(&mut self, pattern: &str) -> Result<BTreeMap<String, SHA256>, BoxError> {
        let paths = self.glob_cache.entry(pattern.to_owned()).or_insert_with(|| {
            let mut result = Vec::new();
            if let Ok(paths) = glob::glob(pattern) {
                for path in paths.flatten() {
                    if path.is_file() {
                        result.push(path);
                    }
                }
            }
            result.sort();
            result
        });
        let mut map = BTreeMap::new();
        // Clone paths to release borrow on self before calling hash_file.
        let paths = paths.clone();
        for path in &paths {
            let hash = self.hash_file(path)?;
            map.insert(path.to_string_lossy().into_owned(), hash);
        }
        Ok(map)
    }

    /// Hash a list of files, returning entries keyed by path.
    pub fn hash_files(&mut self, paths: &[PathBuf]) -> Result<BTreeMap<String, SHA256>, BoxError> {
        let mut map = BTreeMap::new();
        for path in paths {
            let hash = self.hash_file(path)?;
            map.insert(path.to_string_lossy().into_owned(), hash);
        }
        Ok(map)
    }

    /// Clear the file content hash cache. Called after a block's apply so
    /// that a subsequent resolve() sees fresh hashes for files that may
    /// have been produced or modified.
    pub fn clear_hash_cache(&mut self) {
        self.hash_cache.clear();
    }

    /// Reset all caches for a new engine run.
    pub fn reset(&mut self) {
        self.hash_cache.clear();
        self.glob_cache.clear();
    }
}

impl Default for FileTracker {
    fn default() -> Self {
        Self::new()
    }
}
