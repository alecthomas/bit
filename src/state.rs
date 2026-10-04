use std::collections::HashMap;
use std::fs::{self, File};
use std::io::Write;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};

#[derive(Debug, thiserror::Error)]
pub enum StateError {
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("json error: {0}")]
    Json(#[from] serde_json::Error),
    #[error("cannot determine user cache directory")]
    NoCacheDir,
}

/// Persists block state between runs. State is always JSON.
pub trait StateStore: Send + Sync {
    fn load(&self, block: &str) -> Result<Option<Value>, StateError>;
    fn save(&self, block: &str, state: &Value) -> Result<(), StateError>;
    fn remove(&self, block: &str) -> Result<(), StateError>;
    fn list(&self) -> Result<Vec<String>, StateError>;
}

/// JSON file-backed state store. All block states are stored in a single
/// JSON file as a map of block name to state value.
///
/// Concurrent `bit` processes share this file. Writes replace it atomically
/// so readers never see a partial file, and mutations hold an exclusive lock
/// on a sibling `.lock` file so concurrent read-modify-writes don't drop each
/// other's block updates.
pub struct JsonFileStore {
    path: PathBuf,
}

/// On-disk format: map of block name to JSON state.
#[derive(Debug, Default, Serialize, Deserialize)]
struct StoreData {
    blocks: HashMap<String, Value>,
}

impl JsonFileStore {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    fn read_data(&self) -> Result<StoreData, StateError> {
        if !self.path.exists() {
            return Ok(StoreData::default());
        }
        let contents = fs::read_to_string(&self.path)?;
        Ok(serde_json::from_str(&contents)?)
    }

    /// Writes to a temp file in the same directory and renames it over the
    /// state file, so a concurrent reader sees either the old or new
    /// contents. The rename must stay on one filesystem to be atomic.
    fn write_data(&self, data: &StoreData) -> Result<(), StateError> {
        let parent = self.path.parent().unwrap_or(Path::new("."));
        fs::create_dir_all(parent)?;
        let mut tmp = tempfile::NamedTempFile::new_in(parent)?;
        serde_json::to_writer_pretty(&mut tmp, data)?;
        tmp.flush()?;
        tmp.as_file().sync_all()?;
        tmp.persist(&self.path).map_err(|e| e.error)?;
        Ok(())
    }

    /// Takes an exclusive advisory lock covering a whole read-modify-write.
    /// The lock lives on a separate file because `write_data` replaces the
    /// state file's inode, which would orphan a lock held on it. Released
    /// when the returned handle drops.
    fn lock(&self) -> Result<File, StateError> {
        let mut lock_path = self.path.clone().into_os_string();
        lock_path.push(".lock");
        let lock_path = PathBuf::from(lock_path);
        if let Some(parent) = lock_path.parent() {
            fs::create_dir_all(parent)?;
        }
        let file = File::options()
            .create(true)
            .truncate(false)
            .write(true)
            .open(lock_path)?;
        file.lock()?;
        Ok(file)
    }

    fn update(&self, f: impl FnOnce(&mut StoreData)) -> Result<(), StateError> {
        let _lock = self.lock()?;
        let mut data = self.read_data()?;
        f(&mut data);
        self.write_data(&data)
    }
}

impl StateStore for JsonFileStore {
    fn load(&self, block: &str) -> Result<Option<Value>, StateError> {
        let data = self.read_data()?;
        Ok(data.blocks.get(block).cloned())
    }

    fn save(&self, block: &str, state: &Value) -> Result<(), StateError> {
        self.update(|data| {
            data.blocks.insert(block.to_owned(), state.clone());
        })
    }

    fn remove(&self, block: &str) -> Result<(), StateError> {
        self.update(|data| {
            data.blocks.remove(block);
        })
    }

    fn list(&self) -> Result<Vec<String>, StateError> {
        let data = self.read_data()?;
        Ok(data.blocks.keys().cloned().collect())
    }
}

/// Returns a `JsonFileStore` in the user's cache directory, partitioned by
/// a SHA-256 hash of the canonicalized absolute `root` path so each project
/// gets its own state file.
///
/// # Errors
///
/// Returns [`StateError::NoCacheDir`] if the platform cache directory cannot
/// be resolved, or [`StateError::Io`] if `root` cannot be canonicalized.
pub fn default_store(root: &Path) -> Result<JsonFileStore, StateError> {
    let cache = dirs::cache_dir().ok_or(StateError::NoCacheDir)?;
    let abs = fs::canonicalize(root)?;
    // Hash the absolute path so different projects don't collide on a single
    // state file. Hex-encoding `Sha256` output keeps the directory name ASCII.
    let hash = Sha256::digest(abs.as_os_str().as_encoded_bytes());
    let hex: String = hash.iter().map(|b| format!("{b:02x}")).collect();
    Ok(JsonFileStore::new(cache.join("bit").join(hex).join("state.json")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn temp_store() -> (tempfile::TempDir, JsonFileStore) {
        let dir = tempfile::tempdir().unwrap();
        let store = JsonFileStore::new(dir.path().join("state.json"));
        (dir, store)
    }

    #[test]
    fn load_missing_returns_none() {
        let (_dir, store) = temp_store();
        assert!(store.load("foo").unwrap().is_none());
    }

    #[test]
    fn save_and_load() {
        let (_dir, store) = temp_store();
        let state = json!({"output": "/tmp/build", "hash": "abc123"});
        store.save("block1", &state).unwrap();
        assert_eq!(store.load("block1").unwrap().unwrap(), state);
    }

    #[test]
    fn remove_block() {
        let (_dir, store) = temp_store();
        store.save("block1", &json!("data")).unwrap();
        store.remove("block1").unwrap();
        assert!(store.load("block1").unwrap().is_none());
    }

    #[test]
    fn list_blocks() {
        let (_dir, store) = temp_store();
        store.save("a", &json!(1)).unwrap();
        store.save("b", &json!(2)).unwrap();
        let mut blocks = store.list().unwrap();
        blocks.sort();
        assert_eq!(blocks, vec!["a", "b"]);
    }

    #[test]
    fn multiple_blocks_independent() {
        let (_dir, store) = temp_store();
        store.save("x", &json!({"v": "x"})).unwrap();
        store.save("y", &json!({"v": "y"})).unwrap();
        assert_eq!(store.load("x").unwrap().unwrap(), json!({"v": "x"}));
        assert_eq!(store.load("y").unwrap().unwrap(), json!({"v": "y"}));
    }

    #[test]
    fn overwrite_existing() {
        let (_dir, store) = temp_store();
        store.save("block1", &json!("old")).unwrap();
        store.save("block1", &json!("new")).unwrap();
        assert_eq!(store.load("block1").unwrap().unwrap(), json!("new"));
    }

    #[test]
    fn creates_parent_dirs() {
        let dir = tempfile::tempdir().unwrap();
        let store = JsonFileStore::new(dir.path().join("deep/nested/state.json"));
        store.save("block1", &json!("data")).unwrap();
        assert!(dir.path().join("deep/nested/state.json").exists());
    }

    #[test]
    fn concurrent_reader_never_sees_partial_file() {
        let (_dir, store) = temp_store();
        store.save("seed", &json!("x")).unwrap();
        let big = json!("y".repeat(256 * 1024));
        let done = std::sync::atomic::AtomicBool::new(false);
        std::thread::scope(|s| {
            let writer = s.spawn(|| {
                for i in 0..50 {
                    store.save(&format!("b{}", i % 4), &big).unwrap();
                }
                done.store(true, std::sync::atomic::Ordering::Relaxed);
            });
            while !done.load(std::sync::atomic::Ordering::Relaxed) {
                // A truncated or partial file would surface as a JSON error.
                store.list().unwrap();
            }
            writer.join().unwrap();
        });
    }

    #[test]
    fn concurrent_writers_keep_every_update() {
        let (dir, _store) = temp_store();
        let path = dir.path().join("state.json");
        std::thread::scope(|s| {
            for t in 0..8 {
                let path = &path;
                s.spawn(move || {
                    // Separate stores model separate processes sharing the file.
                    let store = JsonFileStore::new(path);
                    for i in 0..10 {
                        store.save(&format!("t{t}-{i}"), &json!(i)).unwrap();
                    }
                });
            }
        });
        assert_eq!(JsonFileStore::new(&path).list().unwrap().len(), 80);
    }

    #[test]
    fn default_store_is_under_cache_dir_and_partitioned() {
        let cache = dirs::cache_dir().expect("cache dir available on test platform");
        let a = tempfile::tempdir().unwrap();
        let b = tempfile::tempdir().unwrap();
        let store_a = default_store(a.path()).unwrap();
        let store_b = default_store(b.path()).unwrap();
        assert!(store_a.path.starts_with(cache.join("bit")));
        assert!(store_b.path.starts_with(cache.join("bit")));
        assert_ne!(store_a.path, store_b.path);
        assert_eq!(store_a.path.file_name().unwrap(), "state.json");
    }
}
