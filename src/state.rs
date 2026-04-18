use std::collections::HashMap;
use std::fs;
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

    fn write_data(&self, data: &StoreData) -> Result<(), StateError> {
        if let Some(parent) = self.path.parent() {
            fs::create_dir_all(parent)?;
        }
        let json = serde_json::to_string_pretty(data)?;
        fs::write(&self.path, json)?;
        Ok(())
    }
}

impl StateStore for JsonFileStore {
    fn load(&self, block: &str) -> Result<Option<Value>, StateError> {
        let data = self.read_data()?;
        Ok(data.blocks.get(block).cloned())
    }

    fn save(&self, block: &str, state: &Value) -> Result<(), StateError> {
        let mut data = self.read_data()?;
        data.blocks.insert(block.to_owned(), state.clone());
        self.write_data(&data)
    }

    fn remove(&self, block: &str) -> Result<(), StateError> {
        let mut data = self.read_data()?;
        data.blocks.remove(block);
        self.write_data(&data)
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
