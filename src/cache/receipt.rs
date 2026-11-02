use std::collections::BTreeMap;
use std::fmt;

use serde::{Deserialize, Serialize};

use super::backend::{Backend, BackendError};
use super::cas::ArtifactRef;
use crate::sha256::SHA256;
use crate::value::Map;

/// Bump when the receipt layout or the action key composition changes.
pub const RECEIPT_VERSION: u32 = 4;

#[derive(Debug, thiserror::Error)]
pub enum ReceiptError {
    #[error(transparent)]
    Backend(#[from] BackendError),
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

/// Every variant published under `key` in `backend`, one per set of
/// recorded variable fingerprints. A missing or malformed set, and variants
/// of another receipt version, are all reported as empty: corruption is a
/// cache miss.
pub fn variants(backend: &dyn Backend, key: &ActionKey) -> Result<Vec<Receipt>, ReceiptError> {
    let Some(bytes) = backend.get_receipts(key)? else {
        return Ok(Vec::new());
    };
    let variants: Vec<Receipt> = serde_json::from_slice(&bytes).unwrap_or_default();
    Ok(variants.into_iter().filter(|r| r.version == RECEIPT_VERSION).collect())
}

/// The first receipt for `key` that `usable` accepts, such as the one whose
/// recorded variables match the current environment.
pub fn load(
    backend: &dyn Backend,
    key: &ActionKey,
    usable: impl Fn(&Receipt) -> bool,
) -> Result<Option<Receipt>, ReceiptError> {
    Ok(variants(backend, key)?.into_iter().find(|r| usable(r)))
}

/// Add `receipt` as a variant of `key`. A variant with the same recorded
/// variables is kept rather than replaced.
///
/// Two processes adding different variants at once can each write a set
/// without the other's, losing one. That costs a later miss, never a wrong
/// result, so no lock is taken.
pub fn publish(backend: &dyn Backend, key: &ActionKey, receipt: &Receipt) -> Result<PublishOutcome, ReceiptError> {
    let mut variants = variants(backend, key)?;
    if let Some(existing) = variants.iter().find(|r| r.variables == receipt.variables) {
        return Ok(if existing == receipt {
            PublishOutcome::AlreadyPresent
        } else {
            PublishOutcome::Conflict
        });
    }
    variants.push(receipt.clone());
    backend.put_receipts(key, &serde_json::to_vec_pretty(&variants)?)?;
    Ok(PublishOutcome::Published)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cache::Disk;

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

    fn disk() -> (tempfile::TempDir, Disk) {
        let dir = tempfile::tempdir().unwrap();
        let disk = Disk::new(dir.path());
        (dir, disk)
    }

    #[test]
    fn publish_and_load() {
        let (_dir, disk) = disk();
        assert!(load(&disk, &key("k"), any).unwrap().is_none());
        assert_eq!(
            publish(&disk, &key("k"), &receipt("a")).unwrap(),
            PublishOutcome::Published
        );
        assert_eq!(load(&disk, &key("k"), any).unwrap().unwrap(), receipt("a"));
        assert!(std::fs::read_dir(disk.receipts_dir()).unwrap().all(|e| {
            let name = e.unwrap().file_name();
            name.to_string_lossy().ends_with(".json")
        }));
    }

    #[test]
    fn conflicting_receipt_keeps_first() {
        let (_dir, disk) = disk();
        publish(&disk, &key("k"), &receipt("a")).unwrap();
        assert_eq!(
            publish(&disk, &key("k"), &receipt("a")).unwrap(),
            PublishOutcome::AlreadyPresent
        );
        assert_eq!(
            publish(&disk, &key("k"), &receipt("b")).unwrap(),
            PublishOutcome::Conflict
        );
        assert_eq!(load(&disk, &key("k"), any).unwrap().unwrap(), receipt("a"));
    }

    #[test]
    fn variants_with_different_recorded_variables_coexist() {
        let (_dir, disk) = disk();
        let gcc = with_cc("built with gcc", "gcc");
        let clang = with_cc("built with clang", "clang");

        assert_eq!(publish(&disk, &key("k"), &gcc).unwrap(), PublishOutcome::Published);
        assert_eq!(publish(&disk, &key("k"), &clang).unwrap(), PublishOutcome::Published);
        assert_eq!(
            publish(&disk, &key("k"), &clang).unwrap(),
            PublishOutcome::AlreadyPresent
        );

        let wants = |cc: &'static str| move |r: &Receipt| r.variables["CC"] == SHA256::digest(cc.as_bytes());
        assert_eq!(load(&disk, &key("k"), wants("gcc")).unwrap().unwrap(), gcc);
        assert_eq!(load(&disk, &key("k"), wants("clang")).unwrap().unwrap(), clang);
        assert!(load(&disk, &key("k"), wants("tcc")).unwrap().is_none());
    }

    #[test]
    fn corrupt_receipt_is_a_miss() {
        let (_dir, disk) = disk();
        disk.put_receipts(&key("k"), b"{ not json").unwrap();
        assert!(load(&disk, &key("k"), any).unwrap().is_none());
    }

    #[test]
    fn wrong_version_is_a_miss() {
        let (_dir, disk) = disk();
        let mut old = receipt("a");
        old.version = RECEIPT_VERSION + 1;
        disk.put_receipts(&key("k"), &serde_json::to_vec(&[old]).unwrap())
            .unwrap();
        assert!(load(&disk, &key("k"), any).unwrap().is_none());
    }
}
