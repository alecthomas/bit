use std::collections::{BTreeMap, HashMap};
use std::fs::Metadata;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::provider::BoxError;
use crate::providers::{LinkChain, hash_file};
use crate::sha256::SHA256;

/// Shared cache for filesystem operations within a single engine run.
///
/// Caches glob expansion and file content hashes so that multiple blocks
/// referencing the same files avoid redundant I/O.
pub struct FileTracker {
    hash_cache: HashMap<PathBuf, SHA256>,
    glob_cache: HashMap<String, Vec<PathBuf>>,
    executables: ExecutableHashes,
}

impl FileTracker {
    pub fn new() -> Self {
        Self {
            hash_cache: HashMap::new(),
            glob_cache: HashMap::new(),
            executables: ExecutableHashes {
                path: None,
                entries: BTreeMap::new(),
                settle: SETTLE,
            },
        }
    }

    /// A tracker whose tool hashes persist in `path` across runs.
    pub fn with_executable_cache(path: PathBuf) -> Self {
        Self {
            executables: ExecutableHashes::load(path),
            ..Self::new()
        }
    }

    /// Hash a tool executable as [`FileTracker::hash_file`] would, without
    /// reading it when its symlink chain and metadata are unchanged since a
    /// previous run hashed it. Tools are large and rarely change, so
    /// reading them on every run would dominate change detection.
    pub fn hash_executable(&mut self, path: &Path) -> Result<SHA256, BoxError> {
        if let Some(hash) = self.hash_cache.get(path) {
            return Ok(*hash);
        }
        let hash = self.executables.hash(path)?;
        self.hash_cache.insert(path.to_path_buf(), hash);
        Ok(hash)
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

    /// Reset all caches for a new engine run. Ambient hashes are kept: each
    /// lookup revalidates them against the filesystem.
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

/// Hashes of tool executables keyed by absolute path, reused while the
/// file's symlink chain and metadata match what was recorded.
struct ExecutableHashes {
    /// Where entries persist; `None` keeps them for this process only.
    path: Option<PathBuf>,
    entries: BTreeMap<String, ExecutableHash>,
    /// How old a file's timestamps must be before its hash is recorded.
    settle: Duration,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ExecutableHash {
    chain: LinkChain,
    stamp: Stamp,
    hash: SHA256,
}

/// Metadata that changes whenever a file's content does. The change time
/// is included because the modification time can be set back, as `cp -p`
/// and archive extraction do, while the change time cannot.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
struct Stamp {
    len: u64,
    modified: (i64, i64),
    changed: (i64, i64),
    inode: u64,
    device: u64,
}

impl Stamp {
    fn of(metadata: &Metadata) -> Self {
        use std::os::unix::fs::MetadataExt;
        Self {
            len: metadata.len(),
            modified: (metadata.mtime(), metadata.mtime_nsec()),
            changed: (metadata.ctime(), metadata.ctime_nsec()),
            inode: metadata.ino(),
            device: metadata.dev(),
        }
    }

    /// Whether either timestamp is within `settle` of now. Filesystems keep
    /// times at limited precision, so a write in the same tick as the hash
    /// would leave the stamp unchanged and the recorded hash stale.
    fn is_recent(&self, settle: Duration) -> bool {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |now| now.as_secs() as i64);
        now - self.modified.0.max(self.changed.0) < settle.as_secs() as i64
    }
}

/// Default for [`ExecutableHashes::settle`]: two seconds covers filesystems that
/// store whole seconds.
const SETTLE: Duration = Duration::from_secs(2);

impl ExecutableHashes {
    /// Entries from `path`. A missing or unreadable file starts empty, since
    /// every entry can be recomputed.
    fn load(path: PathBuf) -> Self {
        let entries = read_entries(&path);
        Self {
            path: Some(path),
            entries,
            settle: SETTLE,
        }
    }

    fn hash(&mut self, path: &Path) -> Result<SHA256, BoxError> {
        let chain = LinkChain::of(path)?;
        let stamp = Stamp::of(&std::fs::metadata(&chain.file)?);
        // Absolute without resolving links, so stubs that share a target
        // keep separate entries.
        let key = std::path::absolute(path)?.to_string_lossy().into_owned();
        if let Some(entry) = self.entries.get(&key)
            && entry.chain == chain
            && entry.stamp == stamp
        {
            return Ok(entry.hash);
        }
        let hash = chain.hash()?;
        if !stamp.is_recent(self.settle) {
            self.entries.insert(key, ExecutableHash { chain, stamp, hash });
            self.save();
        }
        Ok(hash)
    }

    /// Write the entries, merged with any another bit process wrote
    /// meanwhile. Failing to persist only costs a rehash on a later run, so
    /// errors are ignored.
    fn save(&self) {
        let Some(path) = &self.path else { return };
        let mut entries = read_entries(path);
        entries.extend(self.entries.iter().map(|(key, entry)| (key.clone(), entry.clone())));
        let Ok(json) = serde_json::to_vec(&entries) else { return };
        let Some(parent) = path.parent() else { return };
        // Renaming a complete file into place keeps concurrent readers from
        // seeing a partial write.
        let temp = path.with_extension(format!("tmp.{}", std::process::id()));
        if std::fs::create_dir_all(parent).is_ok() && std::fs::write(&temp, json).is_ok() {
            let _ = std::fs::rename(&temp, path);
        }
    }
}

fn read_entries(path: &Path) -> BTreeMap<String, ExecutableHash> {
    std::fs::read(path)
        .ok()
        .and_then(|json| serde_json::from_slice(&json).ok())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::symlink;

    use super::*;

    /// A tracker persisting tool hashes in `dir`, recording even files
    /// written moments ago.
    fn tracker(dir: &Path) -> FileTracker {
        let mut tracker = FileTracker::with_executable_cache(dir.join("executables.json"));
        tracker.executables.settle = Duration::ZERO;
        tracker
    }

    #[test]
    fn hash_tool_matches_hash_file() {
        let dir = tempfile::tempdir().unwrap();
        let tool = dir.path().join("tool");
        std::fs::write(&tool, "v1").unwrap();
        symlink("tool", dir.path().join("stub")).unwrap();
        let mut tracker = tracker(dir.path());
        for path in [tool, dir.path().join("stub")] {
            assert_eq!(tracker.hash_executable(&path).unwrap(), hash_file(&path).unwrap());
        }
    }

    #[test]
    fn unchanged_tool_is_not_read_by_a_later_run() {
        let dir = tempfile::tempdir().unwrap();
        let tool = dir.path().join("tool");
        std::fs::write(&tool, "v1").unwrap();
        tracker(dir.path()).hash_executable(&tool).unwrap();

        // Replace the recorded hash: a later run that returns it cannot
        // have read the file.
        let cache = dir.path().join("executables.json");
        let mut entries = read_entries(&cache);
        let sentinel = SHA256::digest(b"sentinel");
        entries.values_mut().for_each(|entry| entry.hash = sentinel);
        std::fs::write(&cache, serde_json::to_vec(&entries).unwrap()).unwrap();

        assert_eq!(tracker(dir.path()).hash_executable(&tool).unwrap(), sentinel);
    }

    #[test]
    fn changed_content_or_link_target_is_rehashed() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("v1"), "one").unwrap();
        std::fs::write(dir.path().join("v2"), "two").unwrap();
        let stub = dir.path().join("stub");
        symlink("v1", &stub).unwrap();
        let first = tracker(dir.path()).hash_executable(&stub).unwrap();

        std::fs::remove_file(&stub).unwrap();
        symlink("v2", &stub).unwrap();
        let retargeted = tracker(dir.path()).hash_executable(&stub).unwrap();
        assert_ne!(retargeted, first);

        std::fs::write(dir.path().join("v2"), "two, longer").unwrap();
        assert_ne!(tracker(dir.path()).hash_executable(&stub).unwrap(), retargeted);
    }

    #[test]
    fn recently_changed_tool_is_not_recorded() {
        let dir = tempfile::tempdir().unwrap();
        let tool = dir.path().join("tool");
        std::fs::write(&tool, "v1").unwrap();
        let mut tracker = FileTracker::with_executable_cache(dir.path().join("executables.json"));
        tracker.hash_executable(&tool).unwrap();
        assert!(read_entries(&dir.path().join("executables.json")).is_empty());
    }
}
