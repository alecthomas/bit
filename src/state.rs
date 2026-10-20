use std::fs;
use std::io::{BufWriter, ErrorKind};
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

/// JSON state store holding one file per block in a directory.
///
/// A save rewrites only its own block, so saves stay cheap as state grows, and
/// atomic replacement keeps concurrent `bit` processes from conflicting.
pub struct JsonDirStore {
    dir: PathBuf,
}

/// On-disk format of one block's file. File names are hashes, so the name is
/// stored for `list`.
#[derive(Serialize, Deserialize)]
struct BlockFile {
    block: String,
    state: Value,
}

impl JsonDirStore {
    pub fn new(dir: impl Into<PathBuf>) -> Self {
        Self { dir: dir.into() }
    }

    fn block_path(&self, block: &str) -> PathBuf {
        self.dir
            .join(format!("{}.json", hex(&Sha256::digest(block.as_bytes()))))
    }

    fn read(path: &Path) -> Result<Option<BlockFile>, StateError> {
        match fs::read(path) {
            Ok(contents) => Ok(Some(serde_json::from_slice(&contents)?)),
            Err(error) if error.kind() == ErrorKind::NotFound => Ok(None),
            Err(error) => Err(error.into()),
        }
    }

    /// Atomically replaces the block's file.
    ///
    /// The temp file must share the directory for the rename to be atomic.
    fn write(&self, block: &str, state: &Value) -> Result<(), StateError> {
        fs::create_dir_all(&self.dir)?;
        let tmp = tempfile::NamedTempFile::new_in(&self.dir)?;
        let mut writer = BufWriter::new(tmp);
        serde_json::to_writer_pretty(
            &mut writer,
            &BlockFile {
                block: block.to_owned(),
                state: state.clone(),
            },
        )?;
        let tmp = writer.into_inner().map_err(|error| error.into_error())?;
        tmp.as_file().sync_all()?;
        tmp.persist(self.block_path(block)).map_err(|error| error.error)?;
        Ok(())
    }
}

impl StateStore for JsonDirStore {
    fn load(&self, block: &str) -> Result<Option<Value>, StateError> {
        Ok(Self::read(&self.block_path(block))?.map(|file| file.state))
    }

    fn save(&self, block: &str, state: &Value) -> Result<(), StateError> {
        self.write(block, state)
    }

    fn remove(&self, block: &str) -> Result<(), StateError> {
        match fs::remove_file(self.block_path(block)) {
            Err(error) if error.kind() != ErrorKind::NotFound => Err(error.into()),
            _ => Ok(()),
        }
    }

    fn list(&self) -> Result<Vec<String>, StateError> {
        let entries = match fs::read_dir(&self.dir) {
            Ok(entries) => entries,
            Err(error) if error.kind() == ErrorKind::NotFound => return Ok(Vec::new()),
            Err(error) => return Err(error.into()),
        };
        let mut blocks = Vec::new();
        for entry in entries {
            let path = entry?.path();
            if path.extension().is_some_and(|extension| extension == "json")
                && let Some(file) = Self::read(&path)?
            {
                blocks.push(file.block);
            }
        }
        Ok(blocks)
    }
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Returns a `JsonDirStore` in the user's cache directory, partitioned by
/// a SHA-256 hash of the canonicalized absolute `root` path so each project
/// gets its own state directory.
///
/// # Errors
///
/// Returns [`StateError::NoCacheDir`] if the platform cache directory cannot
/// be resolved, or [`StateError::Io`] if `root` cannot be canonicalized.
pub fn default_store(root: &Path) -> Result<JsonDirStore, StateError> {
    let cache = dirs::cache_dir().ok_or(StateError::NoCacheDir)?;
    let abs = fs::canonicalize(root)?;
    // Hash the absolute path so different projects don't collide on a single
    // state directory. Hex-encoding keeps the directory name ASCII.
    let project = hex(&Sha256::digest(abs.as_os_str().as_encoded_bytes()));
    Ok(JsonDirStore::new(cache.join("bit").join(project).join("state")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn temp_store() -> (tempfile::TempDir, JsonDirStore) {
        let dir = tempfile::tempdir().unwrap();
        let store = JsonDirStore::new(dir.path().join("state"));
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
        let store = JsonDirStore::new(dir.path().join("deep/nested/state"));
        store.save("block1", &json!("data")).unwrap();
        assert!(dir.path().join("deep/nested/state").is_dir());
    }

    #[test]
    fn concurrent_reader_never_sees_partial_file() {
        let (_dir, store) = temp_store();
        store.save("b0", &json!("x")).unwrap();
        let big = json!("y".repeat(256 * 1024));
        let done = std::sync::atomic::AtomicBool::new(false);
        std::thread::scope(|s| {
            let writer = s.spawn(|| {
                for _ in 0..50 {
                    store.save("b0", &big).unwrap();
                }
                done.store(true, std::sync::atomic::Ordering::Relaxed);
            });
            while !done.load(std::sync::atomic::Ordering::Relaxed) {
                // A truncated or partial file would surface as a JSON error.
                store.load("b0").unwrap();
            }
            writer.join().unwrap();
        });
    }

    #[test]
    fn concurrent_writers_keep_every_update() {
        let (dir, _store) = temp_store();
        let path = dir.path().join("state");
        std::thread::scope(|s| {
            for t in 0..8 {
                let path = &path;
                s.spawn(move || {
                    // Separate stores model separate processes sharing the file.
                    let store = JsonDirStore::new(path);
                    for i in 0..10 {
                        store.save(&format!("t{t}-{i}"), &json!(i)).unwrap();
                    }
                });
            }
        });
        assert_eq!(JsonDirStore::new(&path).list().unwrap().len(), 80);
    }

    #[test]
    fn default_store_is_under_cache_dir_and_partitioned() {
        let cache = dirs::cache_dir().expect("cache dir available on test platform");
        let a = tempfile::tempdir().unwrap();
        let b = tempfile::tempdir().unwrap();
        let store_a = default_store(a.path()).unwrap();
        let store_b = default_store(b.path()).unwrap();
        assert!(store_a.dir.starts_with(cache.join("bit")));
        assert!(store_b.dir.starts_with(cache.join("bit")));
        assert_ne!(store_a.dir, store_b.dir);
        assert_eq!(store_a.dir.file_name().unwrap(), "state");
    }

    #[test]
    fn save_leaves_other_blocks_untouched() {
        let (_dir, store) = temp_store();
        store.save("a", &json!(1)).unwrap();
        let other = store.block_path("a");
        let before = fs::metadata(&other).unwrap().modified().unwrap();
        store.save("b", &json!(2)).unwrap();
        assert_eq!(fs::metadata(&other).unwrap().modified().unwrap(), before);
    }
}
