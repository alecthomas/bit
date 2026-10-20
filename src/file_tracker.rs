use std::collections::BTreeMap;
use std::fs::Metadata;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use dashmap::DashMap;
use serde::{Deserialize, Serialize};

use crate::provider::BoxError;
use crate::providers::LinkChain;
use crate::sha256::SHA256;

/// Shared cache for filesystem operations within a single engine run.
///
/// Caches glob expansion and file content hashes so that multiple blocks
/// referencing the same files avoid redundant I/O.
///
/// Every block resolves its inputs through one shared tracker, so it is
/// safe to use concurrently and never holds a lock while reading or hashing
/// a file. The maps are sharded and locked only to copy an entry in or out.
/// Two blocks that miss on the same file may both hash it; that costs less
/// than making one wait for the other.
///
/// Cached hashes are never invalidated explicitly. Each lookup compares the
/// file's current symlink chain and metadata with those it was hashed under,
/// so a file a block produces or rewrites is rehashed on its next lookup.
pub struct FileTracker {
    hashes: DashMap<PathBuf, FileHash>,
    globs: DashMap<String, Arc<[PathBuf]>>,
    executables: ExecutableHashes,
    /// How old a file's timestamps must be before its hash is cached.
    settle: Duration,
}

impl FileTracker {
    pub fn new() -> Self {
        Self {
            hashes: DashMap::new(),
            globs: DashMap::new(),
            executables: ExecutableHashes {
                path: None,
                entries: DashMap::new(),
            },
            settle: SETTLE,
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
    pub fn hash_executable(&self, path: &Path) -> Result<SHA256, BoxError> {
        let (chain, stamp) = probe(path)?;
        if let Some(hash) = cached(&self.hashes, path, &chain, &stamp) {
            return Ok(hash);
        }
        // Absolute without resolving links, so stubs that share a target
        // keep separate entries.
        let key = std::path::absolute(path)?.to_string_lossy().into_owned();
        let hash = match cached(&self.executables.entries, &key, &chain, &stamp) {
            Some(hash) => hash,
            None => {
                let hash = chain.hash()?;
                if !stamp.is_recent(self.settle) {
                    self.executables.entries.insert(
                        key,
                        FileHash {
                            chain: chain.clone(),
                            stamp: stamp.clone(),
                            hash,
                        },
                    );
                    self.executables.save();
                }
                hash
            }
        };
        self.remember(path, chain, stamp, hash);
        Ok(hash)
    }

    /// Compute the SHA256 of a file's contents as [`crate::providers::hash_file`]
    /// does, returning a cached result if the file is unchanged.
    pub fn hash_file(&self, path: &Path) -> Result<SHA256, BoxError> {
        let (chain, stamp) = probe(path)?;
        if let Some(hash) = cached(&self.hashes, path, &chain, &stamp) {
            return Ok(hash);
        }
        let hash = chain.hash()?;
        self.remember(path, chain, stamp, hash);
        Ok(hash)
    }

    /// Cache `hash` for `path` unless the file changed too recently for its
    /// stamp to reveal a later write.
    fn remember(&self, path: &Path, chain: LinkChain, stamp: Stamp, hash: SHA256) {
        if !stamp.is_recent(self.settle) {
            self.hashes.insert(path.to_path_buf(), FileHash { chain, stamp, hash });
        }
    }

    /// Expand a glob pattern and hash each matching file.
    /// Returns entries keyed by the file path (relative as matched by glob).
    pub fn hash_glob(&self, pattern: &str) -> Result<BTreeMap<String, SHA256>, BoxError> {
        // Cloned out so no shard lock is held while globbing or hashing.
        let cached = self.globs.get(pattern).map(|paths| Arc::clone(&paths));
        let paths = match cached {
            Some(paths) => paths,
            None => {
                let mut result = Vec::new();
                if let Ok(paths) = glob::glob(pattern) {
                    for path in paths.flatten() {
                        if path.is_file() {
                            result.push(path);
                        }
                    }
                }
                result.sort();
                let paths: Arc<[PathBuf]> = result.into();
                self.globs.insert(pattern.to_owned(), Arc::clone(&paths));
                paths
            }
        };
        self.hash_files(&paths)
    }

    /// Hash a list of discovered files, returning entries keyed by path.
    ///
    /// A file that no longer exists is skipped rather than failing the
    /// block: discovery and hashing are not atomic, and another block may
    /// remove a file in between, such as a temporary file a cache restore
    /// renames into place.
    pub fn hash_files(&self, paths: &[PathBuf]) -> Result<BTreeMap<String, SHA256>, BoxError> {
        let mut map = BTreeMap::new();
        for path in paths {
            match self.hash_file(path) {
                Ok(hash) => {
                    map.insert(path.to_string_lossy().into_owned(), hash);
                }
                Err(e) if is_not_found(e.as_ref()) => {}
                Err(e) => return Err(e),
            }
        }
        Ok(map)
    }

    /// Reset all caches for a new engine run. Ambient hashes are kept: each
    /// lookup revalidates them against the filesystem.
    pub fn reset(&self) {
        self.hashes.clear();
        self.globs.clear();
    }
}

/// `path`'s symlink chain and the metadata of the file it ends at.
fn probe(path: &Path) -> Result<(LinkChain, Stamp), BoxError> {
    let chain = LinkChain::of(path)?;
    let stamp = Stamp::of(&std::fs::metadata(&chain.file)?);
    Ok((chain, stamp))
}

/// The hash `entries` holds for `key`, if it was computed under `chain` and
/// `stamp`. The shard lock is released before returning.
fn cached<K, Q>(entries: &DashMap<K, FileHash>, key: &Q, chain: &LinkChain, stamp: &Stamp) -> Option<SHA256>
where
    K: std::borrow::Borrow<Q> + Eq + std::hash::Hash,
    Q: Eq + std::hash::Hash + ?Sized,
{
    let entry = entries.get(key)?;
    (entry.chain == *chain && entry.stamp == *stamp).then_some(entry.hash)
}

fn is_not_found(error: &(dyn std::error::Error + 'static)) -> bool {
    error
        .downcast_ref::<std::io::Error>()
        .is_some_and(|e| e.kind() == std::io::ErrorKind::NotFound)
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
    entries: DashMap<String, FileHash>,
}

/// A file's hash with the symlink chain and metadata it was computed under,
/// valid while both still match.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct FileHash {
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

/// Default for [`FileTracker::settle`]: two seconds covers filesystems that
/// store whole seconds.
const SETTLE: Duration = Duration::from_secs(2);

impl ExecutableHashes {
    /// Entries from `path`. A missing or unreadable file starts empty, since
    /// every entry can be recomputed.
    fn load(path: PathBuf) -> Self {
        let entries = read_entries(&path).into_iter().collect();
        Self {
            path: Some(path),
            entries,
        }
    }

    /// Write the entries, merged with any another bit process or thread
    /// wrote meanwhile. Failing to persist only costs a rehash on a later
    /// run, so errors are ignored, as is a concurrent save winning the
    /// rename with fewer entries.
    fn save(&self) {
        static SAVES: AtomicU64 = AtomicU64::new(0);
        let Some(path) = &self.path else { return };
        let mut entries = read_entries(path);
        entries.extend(
            self.entries
                .iter()
                .map(|entry| (entry.key().clone(), entry.value().clone())),
        );
        let Ok(json) = serde_json::to_vec(&entries) else { return };
        let Some(parent) = path.parent() else { return };
        // Renaming a complete file into place keeps concurrent readers from
        // seeing a partial write. Each save writes its own temporary file so
        // threads saving at once cannot interleave their writes.
        let save = SAVES.fetch_add(1, Ordering::Relaxed);
        let temp = path.with_extension(format!("tmp.{}.{save}", std::process::id()));
        if std::fs::create_dir_all(parent).is_ok() && std::fs::write(&temp, json).is_ok() {
            let _ = std::fs::rename(&temp, path);
        }
    }
}

fn read_entries(path: &Path) -> BTreeMap<String, FileHash> {
    std::fs::read(path)
        .ok()
        .and_then(|json| serde_json::from_slice(&json).ok())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::symlink;

    use super::*;
    use crate::providers::hash_file;

    /// A tracker persisting tool hashes in `dir`, recording even files
    /// written moments ago.
    fn tracker(dir: &Path) -> FileTracker {
        let mut tracker = FileTracker::with_executable_cache(dir.join("executables.json"));
        tracker.settle = Duration::ZERO;
        tracker
    }

    #[test]
    fn changed_file_is_rehashed_without_clearing() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("file");
        std::fs::write(&file, "one").unwrap();
        let tracker = tracker(dir.path());
        let first = tracker.hash_file(&file).unwrap();
        std::fs::write(&file, "two, longer").unwrap();
        let second = tracker.hash_file(&file).unwrap();
        assert_ne!(second, first);
        assert_eq!(second, hash_file(&file).unwrap());
    }

    #[test]
    fn vanished_files_are_skipped() {
        let dir = tempfile::tempdir().unwrap();
        let kept = dir.path().join("kept.txt");
        let gone = dir.path().join("gone.txt");
        std::fs::write(&kept, "kept").unwrap();
        std::fs::write(&gone, "gone").unwrap();
        let tracker = tracker(dir.path());
        let pattern = format!("{}/*.txt", dir.path().display());
        assert_eq!(tracker.hash_glob(&pattern).unwrap().len(), 2);

        // The glob is cached, so the next expansion still lists the file.
        std::fs::remove_file(&gone).unwrap();
        let hashes = tracker.hash_glob(&pattern).unwrap();
        assert_eq!(
            hashes.keys().collect::<Vec<_>>(),
            [&kept.to_string_lossy().into_owned()]
        );
        assert_eq!(tracker.hash_files(&[gone]).unwrap().len(), 0);
    }

    #[test]
    fn hash_tool_matches_hash_file() {
        let dir = tempfile::tempdir().unwrap();
        let tool = dir.path().join("tool");
        std::fs::write(&tool, "v1").unwrap();
        symlink("tool", dir.path().join("stub")).unwrap();
        let tracker = tracker(dir.path());
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
        let tracker = FileTracker::with_executable_cache(dir.path().join("executables.json"));
        tracker.hash_executable(&tool).unwrap();
        assert!(read_entries(&dir.path().join("executables.json")).is_empty());
    }
}
