//! Durable cross-worktree build cache.
//!
//! Three kinds of state exist:
//!
//! - Worktree-local live state ([`crate::state`]) remains the authority for
//!   planning, drift detection, and `--clean`.
//! - A project-scoped action cache maps an [`ActionKey`] to an immutable
//!   [`Receipt`] describing a successful action.
//! - A global content-addressed store ([`Cas`]) holds the artifact bytes
//!   those receipts reference, outside every worktree.
//!
//! Path strings inside a project are rewritten to project-relative form
//! before hashing so that keys and content hashes agree across worktrees.

pub mod cas;
pub mod project;
pub mod receipt;

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::Serialize;

pub use cas::{ArtifactRef, Cas, CasError};
pub use project::ProjectIdentity;
pub use receipt::{ActionKey, PublishOutcome, RECEIPT_VERSION, Receipt, ReceiptError, ReceiptStore};

use crate::sha256::SHA256;

#[derive(Debug, thiserror::Error)]
pub enum CacheError {
    #[error("cannot determine user cache directory")]
    NoCacheDir,
    #[error(transparent)]
    Receipt(#[from] ReceiptError),
}

/// Everything that determines whether one successful action can be reused.
/// Hashed in canonical JSON form to produce an [`ActionKey`].
#[derive(Debug, Serialize)]
pub struct ActionKeyInput<'a> {
    pub receipt_version: u32,
    pub provider: &'a str,
    pub resource: &'a str,
    pub cache_version: u32,
    /// Expanded block name, including any matrix suffix.
    pub block: &'a str,
    pub os: &'static str,
    pub arch: &'static str,
    /// Hash of the normalized evaluated inputs.
    pub inputs: SHA256,
    /// Normalized provider-resolved source hashes, outputs excluded.
    pub sources: &'a BTreeMap<String, SHA256>,
    /// Result hashes of dependencies selected during the current run.
    pub deps: &'a BTreeMap<String, SHA256>,
    /// Toolchain and environment fingerprint from the provider.
    pub toolchain: &'a BTreeMap<String, String>,
}

impl ActionKeyInput<'_> {
    pub fn key(&self) -> ActionKey {
        let json = serde_json::to_vec(self).unwrap_or_default();
        ActionKey::new(SHA256::digest(&json))
    }
}

/// Environment variable overriding the shared cache location.
pub const CACHE_DIR_ENV: &str = "BIT_CACHE_DIR";

/// Directory holding the shared cache: `$BIT_CACHE_DIR` if set, otherwise
/// `bit` under the platform cache directory.
pub fn cache_root() -> Result<PathBuf, CacheError> {
    if let Some(dir) = std::env::var_os(CACHE_DIR_ENV) {
        return Ok(PathBuf::from(dir));
    }
    Ok(dirs::cache_dir().ok_or(CacheError::NoCacheDir)?.join("bit"))
}

/// Size of the shared cache, for `bit --cache`.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct CacheStats {
    pub receipts: usize,
    pub receipt_bytes: u64,
    pub blobs: usize,
    pub blob_bytes: u64,
}

impl std::fmt::Display for CacheStats {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        writeln!(f, "receipts: {} ({})", self.receipts, human_bytes(self.receipt_bytes))?;
        write!(f, "artifacts: {} ({})", self.blobs, human_bytes(self.blob_bytes))
    }
}

fn human_bytes(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

/// Count files and bytes under `dir`, treating a missing directory as empty.
fn measure(dir: &Path) -> std::io::Result<(usize, u64)> {
    let mut count = 0;
    let mut bytes = 0;
    let mut pending = vec![dir.to_path_buf()];
    while let Some(dir) = pending.pop() {
        let entries = match std::fs::read_dir(&dir) {
            Ok(entries) => entries,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(e) => return Err(e),
        };
        for entry in entries {
            let entry = entry?;
            let meta = entry.metadata()?;
            if meta.is_dir() {
                pending.push(entry.path());
            } else if meta.is_file() {
                count += 1;
                bytes += meta.len();
            }
        }
    }
    Ok((count, bytes))
}

/// Measure the shared cache under `cache_dir`.
pub fn stats(cache_dir: &Path) -> std::io::Result<CacheStats> {
    let (receipts, receipt_bytes) = measure(&cache_dir.join("actions"))?;
    let (blobs, blob_bytes) = measure(&cache_dir.join("cas"))?;
    Ok(CacheStats {
        receipts,
        receipt_bytes,
        blobs,
        blob_bytes,
    })
}

/// Delete every receipt and artifact under `cache_dir`. Worktree-local state
/// and the import cache, which share the directory, are left alone.
pub fn clean(cache_dir: &Path) -> std::io::Result<()> {
    for sub in ["actions", "cas"] {
        match std::fs::remove_dir_all(cache_dir.join(sub)) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e),
        }
    }
    Ok(())
}

struct Shared {
    receipts: ReceiptStore,
    cas: Cas,
}

/// Engine-side view of the cache for one project root.
///
/// Always provides path normalization; receipt lookup and artifact storage
/// are available only when opened with a shared backend.
pub struct BuildCache {
    /// Canonical project root. Providers that emit absolute paths (the Go
    /// scanner) canonicalize them, so a plain prefix comparison suffices.
    root: PathBuf,
    shared: Option<Shared>,
}

impl BuildCache {
    /// A cache that only normalizes paths and never shares results.
    pub fn local_only(root: &Path) -> Self {
        Self {
            root: root.canonicalize().unwrap_or_else(|_| root.to_path_buf()),
            shared: None,
        }
    }

    /// Open the shared cache under [`cache_root`].
    pub fn open(root: &Path) -> Result<Self, CacheError> {
        Ok(Self::open_at(root, &cache_root()?))
    }

    /// Open the shared cache under an explicit cache directory.
    pub fn open_at(root: &Path, cache_dir: &Path) -> Self {
        let mut cache = Self::local_only(root);
        let identity = ProjectIdentity::detect(&cache.root);
        cache.shared = Some(Shared {
            receipts: ReceiptStore::new(cache_dir.join("actions").join("v1").join(identity.id())),
            cas: Cas::new(cache_dir.join("cas").join("v1").join("sha256")),
        });
        cache
    }

    pub fn is_shared(&self) -> bool {
        self.shared.is_some()
    }

    pub fn cas(&self) -> Option<&Cas> {
        self.shared.as_ref().map(|s| &s.cas)
    }

    pub fn receipts(&self) -> Option<&ReceiptStore> {
        self.shared.as_ref().map(|s| &s.receipts)
    }

    pub fn lookup(&self, key: &ActionKey) -> Option<Receipt> {
        self.receipts()?.load(key)
    }

    pub fn publish(&self, key: &ActionKey, receipt: &Receipt) -> Result<PublishOutcome, CacheError> {
        match self.receipts() {
            Some(store) => Ok(store.publish(key, receipt)?),
            None => Ok(PublishOutcome::Published),
        }
    }

    /// Rewrite an absolute path inside the project root to a project-relative
    /// one. Paths outside the project and non-path strings are returned as-is.
    pub fn normalize_str<'a>(&self, s: &'a str) -> std::borrow::Cow<'a, str> {
        let Some(root) = self.root.to_str() else {
            return s.into();
        };
        if s == root {
            return ".".into();
        }
        match s.strip_prefix(root).and_then(|rest| rest.strip_prefix('/')) {
            Some(rel) => rel.into(),
            None => s.into(),
        }
    }

    /// Normalize every string in a JSON value.
    pub fn normalize_json(&self, value: serde_json::Value) -> serde_json::Value {
        use serde_json::Value;
        match value {
            Value::String(s) => Value::String(self.normalize_str(&s).into_owned()),
            Value::Array(items) => Value::Array(items.into_iter().map(|v| self.normalize_json(v)).collect()),
            Value::Object(map) => Value::Object(map.into_iter().map(|(k, v)| (k, self.normalize_json(v))).collect()),
            other => other,
        }
    }

    /// Normalize the keys of a resolve map.
    pub fn normalize_keys(&self, map: &BTreeMap<String, SHA256>) -> BTreeMap<String, SHA256> {
        map.iter()
            .map(|(k, v)| (self.normalize_str(k).into_owned(), *v))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalizes_paths_under_root() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("proj");
        std::fs::create_dir_all(root.join("src")).unwrap();
        let cache = BuildCache::local_only(&root);
        let canonical = root.canonicalize().unwrap();

        assert_eq!(
            cache.normalize_str(&format!("{}/src/main.go", canonical.display())),
            "src/main.go"
        );
        assert_eq!(cache.normalize_str(canonical.to_str().unwrap()), ".");
        assert_eq!(cache.normalize_str("src/main.go"), "src/main.go");
        assert_eq!(cache.normalize_str("/elsewhere/file"), "/elsewhere/file");
        assert_eq!(cache.normalize_str("go build"), "go build");
    }

    #[test]
    fn sibling_with_common_prefix_is_not_normalized() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("proj");
        std::fs::create_dir_all(&root).unwrap();
        let cache = BuildCache::local_only(&root);
        let sibling = format!("{}2/file", root.canonicalize().unwrap().display());
        assert_eq!(cache.normalize_str(&sibling), sibling);
    }

    #[test]
    fn normalizes_json_recursively() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let cache = BuildCache::local_only(&root);
        let value = serde_json::json!({
            "dir": root.to_str().unwrap(),
            "files": [format!("{}/a", root.display()), "b"],
            "n": 1,
        });
        assert_eq!(
            cache.normalize_json(value),
            serde_json::json!({ "dir": ".", "files": ["a", "b"], "n": 1 })
        );
    }

    #[test]
    fn action_key_is_deterministic_and_sensitive() {
        let sources: BTreeMap<String, SHA256> = [("a".to_owned(), SHA256::digest(b"a"))].into();
        let deps = BTreeMap::new();
        let toolchain: BTreeMap<String, String> = [("go".to_owned(), "1.0".to_owned())].into();
        let base = ActionKeyInput {
            receipt_version: RECEIPT_VERSION,
            provider: "go",
            resource: "exe",
            cache_version: 1,
            block: "app",
            os: "linux",
            arch: "x86_64",
            inputs: SHA256::digest(b"inputs"),
            sources: &sources,
            deps: &deps,
            toolchain: &toolchain,
        };
        let same = ActionKeyInput { ..base };
        assert_eq!(base.key(), same.key());
        let other_toolchain: BTreeMap<String, String> = [("go".to_owned(), "1.1".to_owned())].into();
        let different = ActionKeyInput {
            toolchain: &other_toolchain,
            ..base
        };
        assert_ne!(base.key(), different.key());
    }

    #[test]
    fn stats_and_clean_cover_receipts_and_blobs_only() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("proj");
        std::fs::create_dir_all(&root).unwrap();
        let cache_dir = dir.path().join("cache");
        // Unrelated content sharing the cache directory must survive a clean.
        std::fs::create_dir_all(cache_dir.join("abc123")).unwrap();
        std::fs::write(cache_dir.join("abc123/state.json"), "{}").unwrap();

        assert_eq!(stats(&cache_dir).unwrap(), CacheStats::default());

        let cache = BuildCache::open_at(&root, &cache_dir);
        let src = root.join("bin");
        std::fs::write(&src, b"binary").unwrap();
        let artifact = cache.cas().unwrap().put_file(&src).unwrap();
        let receipt = Receipt {
            version: RECEIPT_VERSION,
            provider: "p".into(),
            resource: "r".into(),
            cache_version: 1,
            state: serde_json::Value::Null,
            outputs: crate::value::Map::new(),
            artifacts: [("exe".to_owned(), artifact)].into(),
            content_hash: SHA256::digest(b"x"),
        };
        let key = ActionKey::new(SHA256::digest(b"k"));
        cache.publish(&key, &receipt).unwrap();

        let s = stats(&cache_dir).unwrap();
        assert_eq!((s.receipts, s.blobs, s.blob_bytes), (1, 1, 6));
        assert!(s.receipt_bytes > 0);
        assert!(s.to_string().starts_with("receipts: 1 ("));

        clean(&cache_dir).unwrap();
        assert_eq!(stats(&cache_dir).unwrap(), CacheStats::default());
        assert!(cache_dir.join("abc123/state.json").is_file());
        clean(&cache_dir).unwrap();
    }

    #[test]
    fn human_bytes_units() {
        assert_eq!(human_bytes(0), "0 B");
        assert_eq!(human_bytes(1023), "1023 B");
        assert_eq!(human_bytes(1536), "1.5 KiB");
        assert_eq!(human_bytes(3 * 1024 * 1024), "3.0 MiB");
    }

    #[test]
    fn open_at_creates_project_scoped_store() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("proj");
        std::fs::create_dir_all(&root).unwrap();
        let cache = BuildCache::open_at(&root, &dir.path().join("cache"));
        assert!(cache.is_shared());
        let expected = dir
            .path()
            .join("cache/actions/v1")
            .join(ProjectIdentity::detect(&root).id());
        assert_eq!(cache.receipts().unwrap().dir(), expected);
    }
}
