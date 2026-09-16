use std::collections::BTreeMap;
use std::fmt;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use super::cas::ArtifactRef;
use crate::sha256::SHA256;
use crate::value::Map;

/// Bump when the receipt layout or the action key composition changes.
pub const RECEIPT_VERSION: u32 = 1;

#[derive(Debug, thiserror::Error)]
pub enum ReceiptError {
    #[error("io error: {0}")]
    Io(#[from] io::Error),
    #[error("json error: {0}")]
    Json(#[from] serde_json::Error),
}

/// Identifies one action: everything that determines whether a successful
/// result can be reused. See [`super::ActionKeyInput`] for the composition.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ActionKey(SHA256);

impl ActionKey {
    pub(super) fn new(digest: SHA256) -> Self {
        Self(digest)
    }

    pub fn digest(&self) -> SHA256 {
        self.0
    }
}

impl fmt::Display for ActionKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// Immutable record of a successful action, published under its [`ActionKey`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Receipt {
    pub version: u32,
    pub provider: String,
    pub resource: String,
    pub cache_version: u32,
    /// Provider state as returned by apply.
    pub state: serde_json::Value,
    /// Logical outputs as returned by apply.
    pub outputs: Map,
    /// Durable artifacts by provider-defined role.
    pub artifacts: BTreeMap<String, ArtifactRef>,
    /// Post-apply content hash; dependents use it as their dependency hash.
    pub content_hash: SHA256,
}

/// Outcome of publishing a receipt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PublishOutcome {
    Published,
    /// An identical receipt was already present.
    AlreadyPresent,
    /// A different receipt exists under the same key. The existing receipt
    /// is kept; the caller should warn because this indicates the action key
    /// omits something the result depends on.
    Conflict,
}

/// Project-scoped, append-only store of action receipts. One JSON file per
/// key, published by temp file and rename so readers never see partial JSON.
pub struct ReceiptStore {
    dir: PathBuf,
}

impl ReceiptStore {
    pub fn new(dir: impl Into<PathBuf>) -> Self {
        Self { dir: dir.into() }
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    fn path(&self, key: &ActionKey) -> PathBuf {
        self.dir.join(format!("{key}.json"))
    }

    /// Load the receipt for `key`. Missing, unreadable, or malformed
    /// receipts are all reported as `None`: corruption is a cache miss.
    pub fn load(&self, key: &ActionKey) -> Option<Receipt> {
        let contents = fs::read(self.path(key)).ok()?;
        let receipt: Receipt = serde_json::from_slice(&contents).ok()?;
        (receipt.version == RECEIPT_VERSION).then_some(receipt)
    }

    pub fn publish(&self, key: &ActionKey, receipt: &Receipt) -> Result<PublishOutcome, ReceiptError> {
        let dest = self.path(key);
        if let Some(existing) = self.load(key) {
            return Ok(if existing == *receipt {
                PublishOutcome::AlreadyPresent
            } else {
                PublishOutcome::Conflict
            });
        }
        fs::create_dir_all(&self.dir)?;
        let tmp = self.dir.join(format!("{key}.tmp.{}", std::process::id()));
        let json = serde_json::to_vec_pretty(receipt)?;
        if let Err(e) = fs::write(&tmp, json) {
            let _ = fs::remove_file(&tmp);
            return Err(e.into());
        }
        match fs::rename(&tmp, &dest) {
            Ok(()) => Ok(PublishOutcome::Published),
            Err(e) => {
                let _ = fs::remove_file(&tmp);
                // Lost a race with another publisher of the same key.
                match self.load(key) {
                    Some(existing) if existing == *receipt => Ok(PublishOutcome::AlreadyPresent),
                    Some(_) => Ok(PublishOutcome::Conflict),
                    None => Err(e.into()),
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn receipt(tag: &str) -> Receipt {
        Receipt {
            version: RECEIPT_VERSION,
            provider: "stub".into(),
            resource: "thing".into(),
            cache_version: 1,
            state: serde_json::json!({ "tag": tag }),
            outputs: Map::new(),
            artifacts: BTreeMap::new(),
            content_hash: SHA256::digest(tag.as_bytes()),
        }
    }

    fn key(s: &str) -> ActionKey {
        ActionKey::new(SHA256::digest(s.as_bytes()))
    }

    #[test]
    fn publish_and_load() {
        let dir = tempfile::tempdir().unwrap();
        let store = ReceiptStore::new(dir.path().join("actions"));
        assert!(store.load(&key("k")).is_none());
        assert_eq!(
            store.publish(&key("k"), &receipt("a")).unwrap(),
            PublishOutcome::Published
        );
        assert_eq!(store.load(&key("k")).unwrap(), receipt("a"));
        assert!(fs::read_dir(dir.path().join("actions")).unwrap().all(|e| {
            let name = e.unwrap().file_name();
            name.to_string_lossy().ends_with(".json")
        }));
    }

    #[test]
    fn conflicting_receipt_keeps_first() {
        let dir = tempfile::tempdir().unwrap();
        let store = ReceiptStore::new(dir.path());
        store.publish(&key("k"), &receipt("a")).unwrap();
        assert_eq!(
            store.publish(&key("k"), &receipt("a")).unwrap(),
            PublishOutcome::AlreadyPresent
        );
        assert_eq!(
            store.publish(&key("k"), &receipt("b")).unwrap(),
            PublishOutcome::Conflict
        );
        assert_eq!(store.load(&key("k")).unwrap(), receipt("a"));
    }

    #[test]
    fn corrupt_receipt_is_a_miss() {
        let dir = tempfile::tempdir().unwrap();
        let store = ReceiptStore::new(dir.path());
        store.publish(&key("k"), &receipt("a")).unwrap();
        fs::write(store.path(&key("k")), b"{ not json").unwrap();
        assert!(store.load(&key("k")).is_none());
    }

    #[test]
    fn wrong_version_is_a_miss() {
        let dir = tempfile::tempdir().unwrap();
        let store = ReceiptStore::new(dir.path());
        let mut old = receipt("a");
        old.version = RECEIPT_VERSION + 1;
        fs::create_dir_all(dir.path()).unwrap();
        fs::write(store.path(&key("k")), serde_json::to_vec(&old).unwrap()).unwrap();
        assert!(store.load(&key("k")).is_none());
    }
}
