//! Durable cross-worktree build cache.
//!
//! Three kinds of state exist:
//!
//! - Worktree-local live state ([`crate::state`]) remains the authority for
//!   planning, drift detection, and `--clean`.
//! - An action cache maps an [`ActionKey`] to immutable [`Receipt`]s
//!   describing successful actions.
//! - A content-addressed store ([`Cas`]) holds the artifact bytes those
//!   receipts reference, outside every worktree.
//!
//! Both are global rather than per project: path strings are rewritten by
//! [`PortablePaths`] before hashing, so a key is the same wherever the same
//! action runs, and any checkout can reuse it.
//!
//! Both live on local [`Disk`], and optionally also in a [`Remote`] behind
//! it; see [`Backend`].

pub mod backend;
pub mod cas;
pub mod disk;
pub mod location;
pub mod project;
pub mod receipt;
pub mod s3;
mod trim;

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

use serde::Serialize;

pub use backend::{Backend, BackendError};
pub use cas::{ArtifactRef, Cas, CasError};
pub use disk::Disk;
pub use location::{Location, LocationError};
pub use project::PortablePaths;
pub use receipt::{ActionKey, PublishOutcome, RECEIPT_VERSION, Receipt, ReceiptError};
pub use s3::S3;

use crate::sha256::SHA256;

#[derive(Debug, thiserror::Error)]
pub enum CacheError {
    #[error("cannot determine user cache directory")]
    NoCacheDir,
    #[error(transparent)]
    Receipt(#[from] ReceiptError),
    #[error(transparent)]
    Backend(#[from] BackendError),
    #[error(transparent)]
    Cas(#[from] CasError),
    /// A failure reaching or reading the remote cache rather than the disk.
    #[error("remote cache: {0}")]
    Remote(Box<CacheError>),
}

impl CacheError {
    fn remote(e: impl Into<CacheError>) -> Self {
        Self::Remote(Box::new(e.into()))
    }

    pub fn is_remote(&self) -> bool {
        matches!(self, Self::Remote(_))
    }
}

/// Everything that determines whether one successful action can be reused.
/// Hashed in canonical JSON form to produce an [`ActionKey`].
#[derive(Debug, Serialize)]
pub struct ActionKeyInput<'a> {
    pub receipt_version: u32,
    pub provider: &'a str,
    pub resource: &'a str,
    pub cache_version: u32,
    /// Expanded target name, including any matrix suffix.
    pub target: &'a str,
    pub os: &'static str,
    pub arch: &'static str,
    /// Hash of the normalized evaluated inputs.
    pub inputs: SHA256,
    /// Normalized provider-resolved source hashes, including any source that
    /// is also a declared output.
    pub sources: &'a BTreeMap<String, SHA256>,
    /// Result hashes of dependencies selected during the current run.
    pub deps: &'a BTreeMap<String, SHA256>,
}

impl ActionKeyInput<'_> {
    pub fn key(&self) -> ActionKey {
        let json = serde_json::to_vec(self).unwrap_or_default();
        ActionKey::new(SHA256::digest(&json))
    }
}

/// `bit` under the platform cache directory: where the disk cache lives
/// unless `disk://<path>` moves it, and where bit keeps other per-user
/// state, such as executable hashes, wherever the cache is.
pub fn default_dir() -> Result<PathBuf, CacheError> {
    Ok(dirs::cache_dir().ok_or(CacheError::NoCacheDir)?.join("bit"))
}

/// The backend `location` names. For an S3 location that is the bucket
/// alone, not the disk cache that a build puts in front of it.
pub fn backend(location: &Location) -> Result<Box<dyn Backend>, CacheError> {
    Ok(match location {
        Location::Disk(Some(dir)) => Box::new(Disk::new(dir)),
        Location::Disk(None) => Box::new(Disk::new(&default_dir()?)),
        Location::S3 { bucket, prefix, .. } => Box::new(S3::new(bucket, prefix)),
    })
}

/// A cache behind the local disk cache, such as one shared by a team.
pub struct Remote {
    backend: Box<dyn Backend>,
    /// Whether results published locally are also published here. Anyone
    /// who can write to a cache decides what every reader restores, so a
    /// remote is only written when configured to be.
    writable: bool,
}

impl Remote {
    pub fn new(backend: Box<dyn Backend>, writable: bool) -> Self {
        Self { backend, writable }
    }
}

struct Shared {
    disk: Disk,
    remote: Option<Remote>,
}

impl Shared {
    /// Copy the blob for `digest` from `remote` to disk unless it is there
    /// already. The download is checked against the digest before it is
    /// published, so a corrupt one is rejected.
    fn download(&self, remote: &Remote, digest: &SHA256) -> Result<(), CacheError> {
        let cas = self.disk.cas();
        if cas.contains(digest) {
            return Ok(());
        }
        let tmp = cas.staging_file()?;
        match remote.backend.get_blob(digest, &tmp) {
            Ok(true) => Ok(cas.adopt(digest, &tmp)?),
            Ok(false) => {
                let _ = std::fs::remove_file(&tmp);
                Err(CasError::Missing(*digest).into())
            }
            Err(e) => {
                let _ = std::fs::remove_file(&tmp);
                Err(e.into())
            }
        }
    }
}

/// Rewrite an absolute path inside `root` to one relative to it, with `root`
/// itself as `.`. Paths outside `root` and non-path strings are returned
/// as-is. `root` must be canonical to match the canonical paths providers
/// report.
pub fn project_relative<'a>(root: &Path, s: &'a str) -> std::borrow::Cow<'a, str> {
    let Some(root) = root.to_str() else {
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

/// Engine-side view of the cache for one project root.
///
/// Always provides path normalization; receipt lookup and artifact storage
/// are available only when opened with a shared backend.
pub struct BuildCache {
    paths: PortablePaths,
    shared: Option<Shared>,
    /// Set once the remote has failed, so a run reports that once rather
    /// than for every target.
    remote_failed: AtomicBool,
}

impl BuildCache {
    /// A cache that only normalizes paths and never shares results.
    pub fn local_only(root: &Path) -> Self {
        Self {
            paths: PortablePaths::detect(root),
            shared: None,
            remote_failed: AtomicBool::new(false),
        }
    }

    pub fn paths(&self) -> &PortablePaths {
        &self.paths
    }

    /// Open the cache at `location`: on disk, or on the default disk with an
    /// S3 bucket behind it.
    pub fn open(root: &Path, location: &Location) -> Result<Self, CacheError> {
        Ok(match location {
            Location::Disk(Some(dir)) => Self::open_at(root, dir),
            Location::Disk(None) => Self::open_at(root, &default_dir()?),
            Location::S3 { writable, .. } => {
                Self::open_at(root, &default_dir()?).with_remote(Remote::new(backend(location)?, *writable))
            }
        })
    }

    /// Whether `error` is the first remote failure this run, which callers
    /// report; later ones would only repeat it.
    pub fn first_remote_failure(&self, error: &CacheError) -> bool {
        // The flag orders nothing else; it only decides who reports.
        error.is_remote() && !self.remote_failed.swap(true, Ordering::Relaxed)
    }

    /// Open the shared cache under an explicit cache directory.
    pub fn open_at(root: &Path, cache_dir: &Path) -> Self {
        let mut cache = Self::local_only(root);
        cache.shared = Some(Shared {
            disk: Disk::new(cache_dir),
            remote: None,
        });
        cache
    }

    /// Put `remote` behind the disk cache. A cache that never shares
    /// results ignores it.
    pub fn with_remote(mut self, remote: Remote) -> Self {
        if let Some(shared) = &mut self.shared {
            shared.remote = Some(remote);
        }
        self
    }

    pub fn is_shared(&self) -> bool {
        self.shared.is_some()
    }

    /// The local artifact store providers restore files from.
    pub fn cas(&self) -> Option<&Cas> {
        self.shared.as_ref().map(|s| s.disk.cas())
    }

    /// The first receipt under `key` that `usable` accepts, from disk or
    /// else from the remote. The remote's receipts are copied to disk, so
    /// the next lookup is local; failing to copy them only costs another
    /// remote lookup. The receipt found is marked used, so the disk cache
    /// keeps it; see [`BuildCache::trim`].
    pub fn lookup(&self, key: &ActionKey, usable: impl Fn(&Receipt) -> bool) -> Result<Option<Receipt>, CacheError> {
        let Some(shared) = &self.shared else {
            return Ok(None);
        };
        if let Some(receipt) = receipt::load(&shared.disk, key, &usable)? {
            shared.disk.mark_used(key, &receipt);
            return Ok(Some(receipt));
        }
        let Some(remote) = &shared.remote else {
            return Ok(None);
        };
        let variants = receipt::variants(remote.backend.as_ref(), key).map_err(CacheError::remote)?;
        let found = variants.iter().find(|r| usable(r)).cloned();
        for variant in &variants {
            let _ = receipt::publish(&shared.disk, key, variant);
        }
        if let Some(receipt) = &found {
            shared.disk.mark_used(key, receipt);
        }
        Ok(found)
    }

    /// Remove what the disk cache has not used for days, at most once a
    /// day. Runs in the calling process, as Go's build cache does, but
    /// costs one small read whenever no trim is due, and never waits for
    /// another process's trim. A remote is never trimmed.
    pub fn trim(&self) {
        if let Some(shared) = &self.shared {
            shared.disk.trim();
        }
    }

    /// Copy the blobs `artifacts` need from the remote to disk, where
    /// providers restore them from. A tree's manifest is always copied,
    /// because checking a receipt reads it; with `contents`, so are the
    /// files themselves, which only restoring needs, so planning never
    /// downloads them.
    pub fn fetch<'a>(
        &self,
        artifacts: impl IntoIterator<Item = &'a ArtifactRef>,
        contents: bool,
    ) -> Result<(), CacheError> {
        let Some(shared) = &self.shared else {
            return Ok(());
        };
        let Some(remote) = &shared.remote else {
            return Ok(());
        };
        for artifact in artifacts {
            if let ArtifactRef::Tree { manifest, .. } = artifact {
                shared.download(remote, manifest).map_err(CacheError::remote)?;
            }
            if contents {
                for digest in shared.disk.cas().blobs(artifact)? {
                    shared.download(remote, &digest).map_err(CacheError::remote)?;
                }
            }
        }
        Ok(())
    }

    /// Publish `receipt` to disk. See [`BuildCache::upload`] for the remote.
    pub fn publish(&self, key: &ActionKey, receipt: &Receipt) -> Result<PublishOutcome, CacheError> {
        match &self.shared {
            Some(shared) => Ok(receipt::publish(&shared.disk, key, receipt)?),
            None => Ok(PublishOutcome::Published),
        }
    }

    /// Publish `receipt`, already published to disk, to a writable remote:
    /// its blobs first, so the remote never holds a receipt whose
    /// artifacts it lacks. Returns `None` when there is no writable remote.
    pub fn upload(&self, key: &ActionKey, receipt: &Receipt) -> Result<Option<PublishOutcome>, CacheError> {
        let Some(shared) = &self.shared else {
            return Ok(None);
        };
        let Some(remote) = shared.remote.as_ref().filter(|remote| remote.writable) else {
            return Ok(None);
        };
        let cas = shared.disk.cas();
        for artifact in receipt.artifacts.values() {
            for digest in cas.blobs(artifact)? {
                if !remote.backend.has_blob(&digest).map_err(CacheError::remote)? {
                    remote
                        .backend
                        .put_blob(&digest, &cas.blob_path(&digest))
                        .map_err(CacheError::remote)?;
                }
            }
        }
        let outcome = receipt::publish(remote.backend.as_ref(), key, receipt).map_err(CacheError::remote)?;
        Ok(Some(outcome))
    }

    /// The portable form of a path a provider reports; see
    /// [`PortablePaths::portable_path`].
    pub fn normalize_str<'a>(&self, s: &'a str) -> std::borrow::Cow<'a, str> {
        self.paths.portable_path(s)
    }

    /// Make a path [`BuildCache::normalize_str`] produced absolute again.
    pub fn absolute(&self, s: &str) -> PathBuf {
        self.paths.absolute(s)
    }

    /// Rewrite every absolute path in a JSON value. Relative strings are
    /// left alone: they may not be paths at all.
    pub fn normalize_json(&self, value: serde_json::Value) -> serde_json::Value {
        use serde_json::Value;
        match value {
            Value::String(s) => Value::String(self.paths.portable(&s).into_owned()),
            Value::Array(items) => Value::Array(items.into_iter().map(|v| self.normalize_json(v)).collect()),
            Value::Object(map) => Value::Object(map.into_iter().map(|(k, v)| (k, self.normalize_json(v))).collect()),
            other => other,
        }
    }

    /// Normalize the path keys of a resolve map. Ambient fingerprint keys
    /// are not paths and are kept as they are.
    pub fn normalize_keys(&self, map: &BTreeMap<String, SHA256>) -> BTreeMap<String, SHA256> {
        map.iter()
            .map(|(k, v)| {
                let key = if crate::providers::is_ambient_key(k) {
                    k.clone()
                } else {
                    self.normalize_str(k).into_owned()
                };
                (key, *v)
            })
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
        let base = ActionKeyInput {
            receipt_version: RECEIPT_VERSION,
            provider: "go",
            resource: "exe",
            cache_version: 1,
            target: "app",
            os: "linux",
            arch: "x86_64",
            inputs: SHA256::digest(b"inputs"),
            sources: &sources,
            deps: &deps,
        };
        let same = ActionKeyInput { ..base };
        assert_eq!(base.key(), same.key());
        let other_sources: BTreeMap<String, SHA256> = [("a".to_owned(), SHA256::digest(b"b"))].into();
        let different = ActionKeyInput {
            sources: &other_sources,
            ..base
        };
        assert_ne!(base.key(), different.key());
    }

    #[test]
    fn clean_removes_receipts_and_blobs_only() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("proj");
        std::fs::create_dir_all(&root).unwrap();
        let cache_dir = dir.path().join("cache");
        // Unrelated content sharing the cache directory must survive a clean.
        std::fs::create_dir_all(cache_dir.join("abc123")).unwrap();
        std::fs::write(cache_dir.join("abc123/state.json"), "{}").unwrap();

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
            variables: BTreeMap::new(),
        };
        let key = ActionKey::new(SHA256::digest(b"k"));
        cache.publish(&key, &receipt).unwrap();

        let disk = backend(&Location::Disk(Some(cache_dir.clone()))).unwrap();
        disk.clean().unwrap();
        assert!(!cache_dir.join("actions").exists());
        assert!(!cache_dir.join("cas").exists());
        assert!(cache_dir.join("abc123/state.json").is_file());
        disk.clean().unwrap();
    }

    #[test]
    fn a_remote_cache_is_never_cleaned() {
        let s3 = backend(&"s3+rw://bucket/prefix".parse().unwrap()).unwrap();
        assert!(matches!(s3.clean(), Err(BackendError::CannotClean)));
    }

    #[test]
    fn only_the_first_remote_failure_is_reported() {
        let dir = tempfile::tempdir().unwrap();
        let cache = BuildCache::local_only(dir.path());
        let remote = CacheError::remote(CasError::Missing(SHA256::digest(b"x")));
        let local = CacheError::Cas(CasError::Missing(SHA256::digest(b"x")));
        assert!(!cache.first_remote_failure(&local));
        assert!(cache.first_remote_failure(&remote));
        assert!(!cache.first_remote_failure(&remote));
    }

    #[test]
    fn open_at_shares_one_store_between_projects() {
        let dir = tempfile::tempdir().unwrap();
        let cache_dir = dir.path().join("cache");
        let receipts = |name: &str| {
            let root = dir.path().join(name);
            std::fs::create_dir_all(&root).unwrap();
            let cache = BuildCache::open_at(&root, &cache_dir);
            assert!(cache.is_shared());
            cache.shared.unwrap().disk.receipts_dir().to_path_buf()
        };
        assert_eq!(receipts("a"), cache_dir.join("actions/v2"));
        assert_eq!(receipts("a"), receipts("b"));
    }

    fn receipt_with(artifacts: BTreeMap<String, ArtifactRef>) -> Receipt {
        Receipt {
            version: RECEIPT_VERSION,
            provider: "p".into(),
            resource: "r".into(),
            cache_version: 1,
            state: serde_json::Value::Null,
            outputs: crate::value::Map::new(),
            artifacts,
            content_hash: SHA256::digest(b"x"),
            variables: BTreeMap::new(),
        }
    }

    /// Two machines' caches, each with its own disk, in front of one remote
    /// that a third directory stands in for.
    struct Machines {
        dir: tempfile::TempDir,
    }

    impl Machines {
        fn new() -> Self {
            let dir = tempfile::tempdir().unwrap();
            std::fs::create_dir_all(dir.path().join("proj")).unwrap();
            Self { dir }
        }

        fn cache(&self, machine: &str, writable: bool) -> BuildCache {
            let remote = Disk::new(&self.dir.path().join("remote"));
            BuildCache::open_at(&self.dir.path().join("proj"), &self.dir.path().join(machine))
                .with_remote(Remote::new(Box::new(remote), writable))
        }

        fn remote(&self) -> Disk {
            Disk::new(&self.dir.path().join("remote"))
        }

        fn write(&self, rel: &str, contents: &str) -> PathBuf {
            let path = self.dir.path().join("proj").join(rel);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(&path, contents).unwrap();
            path
        }
    }

    fn any(_: &Receipt) -> bool {
        true
    }

    #[test]
    fn a_result_uploaded_from_one_machine_is_used_on_another() {
        let m = Machines::new();
        let ci = m.cache("ci", true);
        let exe = ci.cas().unwrap().put_file(&m.write("bin", "binary")).unwrap();
        m.write("dist/index.js", "main");
        let dist = ci.cas().unwrap().put_tree(&m.dir.path().join("proj/dist")).unwrap();
        let receipt = receipt_with([("exe".to_owned(), exe.clone()), ("dist".to_owned(), dist.clone())].into());
        let key = ActionKey::new(SHA256::digest(b"k"));
        ci.publish(&key, &receipt).unwrap();
        assert_eq!(ci.upload(&key, &receipt).unwrap(), Some(PublishOutcome::Published));

        let dev = m.cache("dev", false);
        assert_eq!(dev.lookup(&key, any).unwrap(), Some(receipt.clone()));
        let dev_cas = dev.cas().unwrap();
        let ArtifactRef::Tree { manifest, .. } = &dist else {
            unreachable!()
        };

        dev.fetch(receipt.artifacts.values(), false).unwrap();
        assert!(dev_cas.contains(manifest), "checking a receipt needs the manifest");
        assert!(
            !dev_cas.contains(&exe.file().unwrap().digest),
            "planning downloads no contents"
        );

        dev.fetch(receipt.artifacts.values(), true).unwrap();
        for artifact in receipt.artifacts.values() {
            for digest in dev_cas.blobs(artifact).unwrap() {
                assert!(dev_cas.contains(&digest));
            }
        }

        // The receipt was copied to disk, so the remote is no longer needed.
        let offline = BuildCache::open_at(&m.dir.path().join("proj"), &m.dir.path().join("dev"));
        assert_eq!(offline.lookup(&key, any).unwrap(), Some(receipt));
    }

    #[test]
    fn a_read_only_remote_is_never_written() {
        let m = Machines::new();
        let dev = m.cache("dev", false);
        let exe = dev.cas().unwrap().put_file(&m.write("bin", "binary")).unwrap();
        let receipt = receipt_with([("exe".to_owned(), exe.clone())].into());
        let key = ActionKey::new(SHA256::digest(b"k"));
        dev.publish(&key, &receipt).unwrap();

        assert_eq!(dev.upload(&key, &receipt).unwrap(), None);
        assert!(m.remote().get_receipts(&key).unwrap().is_none());
        assert!(!m.remote().has_blob(&exe.file().unwrap().digest).unwrap());
    }

    #[test]
    fn a_corrupt_remote_blob_is_rejected() {
        let m = Machines::new();
        let ci = m.cache("ci", true);
        let exe = ci.cas().unwrap().put_file(&m.write("bin", "binary")).unwrap();
        let receipt = receipt_with([("exe".to_owned(), exe.clone())].into());
        let key = ActionKey::new(SHA256::digest(b"k"));
        ci.publish(&key, &receipt).unwrap();
        ci.upload(&key, &receipt).unwrap();
        let digest = exe.file().unwrap().digest;
        let blob = m.remote().cas().blob_path(&digest);
        std::fs::set_permissions(&blob, std::os::unix::fs::PermissionsExt::from_mode(0o644)).unwrap();
        std::fs::write(&blob, "tampered").unwrap();

        let dev = m.cache("dev", false);
        let err = dev.fetch(receipt.artifacts.values(), true).unwrap_err();
        assert!(
            matches!(&err, CacheError::Remote(inner) if matches!(**inner, CacheError::Cas(CasError::Corrupt { .. }))),
            "{err}"
        );
        assert!(!dev.cas().unwrap().contains(&digest));
    }

    #[test]
    fn trim_keeps_what_lookups_used_and_removes_the_rest() {
        let m = Machines::new();
        let cache_dir = m.dir.path().join("dev");
        let cache = BuildCache::open_at(&m.dir.path().join("proj"), &cache_dir);
        let cas = cache.cas().unwrap();
        m.write("dist/index.js", "main");
        let dist = cas.put_tree(&m.dir.path().join("proj/dist")).unwrap();
        let used = receipt_with([("dist".to_owned(), dist.clone())].into());
        let stale_exe = cas.put_file(&m.write("old", "old binary")).unwrap();
        let stale = receipt_with([("exe".to_owned(), stale_exe.clone())].into());
        let (used_key, stale_key) = (
            ActionKey::new(SHA256::digest(b"used")),
            ActionKey::new(SHA256::digest(b"stale")),
        );
        cache.publish(&used_key, &used).unwrap();
        cache.publish(&stale_key, &stale).unwrap();

        let disk = &cache.shared.as_ref().unwrap().disk;
        let mut files = vec![disk.receipts_path(&used_key), disk.receipts_path(&stale_key)];
        let used_blobs: Vec<_> = cas.blobs(&dist).unwrap().iter().map(|d| cas.blob_path(d)).collect();
        let stale_blob = cas.blob_path(&stale_exe.file().unwrap().digest);
        files.extend(used_blobs.iter().cloned());
        files.push(stale_blob.clone());
        for file in &files {
            trim::tests::backdate(file, std::time::Duration::from_secs(10 * 24 * 60 * 60));
        }

        assert_eq!(cache.lookup(&used_key, any).unwrap(), Some(used));
        cache.trim();

        assert!(disk.receipts_path(&used_key).is_file());
        assert!(
            used_blobs.iter().all(|blob| blob.is_file()),
            "a tree keeps its manifest and files"
        );
        assert!(!disk.receipts_path(&stale_key).exists());
        assert!(!stale_blob.exists());
        assert!(cache_dir.join("trim.txt").is_file());
    }

    #[test]
    fn a_blob_missing_from_the_remote_is_reported() {
        let m = Machines::new();
        let dev = m.cache("dev", false);
        let missing = ArtifactRef::File(cas::FileRef {
            digest: SHA256::digest(b"nope"),
            size: 4,
            mode: 0o644,
        });
        let err = dev.fetch([&missing], true).unwrap_err();
        assert!(
            matches!(&err, CacheError::Remote(inner) if matches!(**inner, CacheError::Cas(CasError::Missing(_)))),
            "{err}"
        );
    }
}
