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
pub const RECEIPT_VERSION: u32 = 4;

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
    /// Fingerprints of the environment variables the tool recorded reading,
    /// by name. They are known only once the action has run, so they cannot
    /// be part of the [`ActionKey`] that is computed before it; instead the
    /// receipt is reused only where they still match.
    pub variables: BTreeMap<String, SHA256>,
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
/// key holds every variant published under it, one per set of recorded
/// variable fingerprints. The file is replaced by temp file and rename so
/// readers never see partial JSON.
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

    /// Every variant published under `key`. Missing, unreadable, or malformed
    /// files are all reported as empty: corruption is a cache miss.
    fn variants(&self, key: &ActionKey) -> Vec<Receipt> {
        let Ok(contents) = fs::read(self.path(key)) else {
            return Vec::new();
        };
        let variants: Vec<Receipt> = serde_json::from_slice(&contents).unwrap_or_default();
        variants.into_iter().filter(|r| r.version == RECEIPT_VERSION).collect()
    }

    /// Load the first receipt for `key` that `usable` accepts, such as the
    /// one whose recorded variables match the current environment.
    pub fn load(&self, key: &ActionKey, usable: impl Fn(&Receipt) -> bool) -> Option<Receipt> {
        self.variants(key).into_iter().find(|r| usable(r))
    }

    /// Add `receipt` as a variant of `key`. A variant with the same recorded
    /// variables is kept rather than replaced.
    ///
    /// Two processes adding different variants at once can each write a
    /// file without the other's, losing one. That costs a later miss, never
    /// a wrong result, so no lock is taken.
    pub fn publish(&self, key: &ActionKey, receipt: &Receipt) -> Result<PublishOutcome, ReceiptError> {
        let mut variants = self.variants(key);
        if let Some(existing) = variants.iter().find(|r| r.variables == receipt.variables) {
            return Ok(if existing == receipt {
                PublishOutcome::AlreadyPresent
            } else {
                PublishOutcome::Conflict
            });
        }
        variants.push(receipt.clone());
        fs::create_dir_all(&self.dir)?;
        let tmp = self.dir.join(format!("{key}.tmp.{}", std::process::id()));
        let json = serde_json::to_vec_pretty(&variants)?;
        if let Err(e) = fs::write(&tmp, json) {
            let _ = fs::remove_file(&tmp);
            return Err(e.into());
        }
        if let Err(e) = fs::rename(&tmp, self.path(key)) {
            let _ = fs::remove_file(&tmp);
            return Err(e.into());
        }
        Ok(PublishOutcome::Published)
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
            variables: BTreeMap::new(),
        }
    }

    /// A receipt recording that the tool read `CC` with the given value.
    fn with_cc(tag: &str, cc: &str) -> Receipt {
        Receipt {
            variables: [("CC".to_owned(), SHA256::digest(cc.as_bytes()))].into(),
            ..receipt(tag)
        }
    }

    fn key(s: &str) -> ActionKey {
        ActionKey::new(SHA256::digest(s.as_bytes()))
    }

    fn any(_: &Receipt) -> bool {
        true
    }

    #[test]
    fn publish_and_load() {
        let dir = tempfile::tempdir().unwrap();
        let store = ReceiptStore::new(dir.path().join("actions"));
        assert!(store.load(&key("k"), any).is_none());
        assert_eq!(
            store.publish(&key("k"), &receipt("a")).unwrap(),
            PublishOutcome::Published
        );
        assert_eq!(store.load(&key("k"), any).unwrap(), receipt("a"));
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
        assert_eq!(store.load(&key("k"), any).unwrap(), receipt("a"));
    }

    #[test]
    fn variants_with_different_recorded_variables_coexist() {
        let dir = tempfile::tempdir().unwrap();
        let store = ReceiptStore::new(dir.path());
        let gcc = with_cc("built with gcc", "gcc");
        let clang = with_cc("built with clang", "clang");

        assert_eq!(store.publish(&key("k"), &gcc).unwrap(), PublishOutcome::Published);
        assert_eq!(store.publish(&key("k"), &clang).unwrap(), PublishOutcome::Published);
        assert_eq!(
            store.publish(&key("k"), &clang).unwrap(),
            PublishOutcome::AlreadyPresent
        );

        let wants = |cc: &'static str| move |r: &Receipt| r.variables["CC"] == SHA256::digest(cc.as_bytes());
        assert_eq!(store.load(&key("k"), wants("gcc")).unwrap(), gcc);
        assert_eq!(store.load(&key("k"), wants("clang")).unwrap(), clang);
        assert!(store.load(&key("k"), wants("tcc")).is_none());
    }

    #[test]
    fn corrupt_receipt_is_a_miss() {
        let dir = tempfile::tempdir().unwrap();
        let store = ReceiptStore::new(dir.path());
        store.publish(&key("k"), &receipt("a")).unwrap();
        fs::write(store.path(&key("k")), b"{ not json").unwrap();
        assert!(store.load(&key("k"), any).is_none());
    }

    #[test]
    fn wrong_version_is_a_miss() {
        let dir = tempfile::tempdir().unwrap();
        let store = ReceiptStore::new(dir.path());
        let mut old = receipt("a");
        old.version = RECEIPT_VERSION + 1;
        fs::create_dir_all(dir.path()).unwrap();
        fs::write(store.path(&key("k")), serde_json::to_vec(&[old]).unwrap()).unwrap();
        assert!(store.load(&key("k"), any).is_none());
    }
}
