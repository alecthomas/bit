use std::fs;
use std::io::{self, Read};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::sha256::{Hasher, SHA256};

#[derive(Debug, thiserror::Error)]
pub enum CasError {
    #[error("io error: {0}")]
    Io(#[from] io::Error),
    #[error("artifact {0} is not in the cache")]
    Missing(SHA256),
    #[error("artifact {expected} is corrupt (content hashes to {actual})")]
    Corrupt { expected: SHA256, actual: SHA256 },
}

/// A durable artifact stored in the CAS, as referenced by an action receipt.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ArtifactRef {
    pub digest: SHA256,
    pub size: u64,
    /// Unix permission bits to apply when the artifact is materialized.
    pub mode: u32,
}

/// Global content-addressed store of immutable file blobs.
///
/// Blobs live at `<root>/<first two hex chars>/<digest>` and are published
/// by writing to a temporary sibling and renaming into place, so readers
/// never observe a partial blob. Blobs are stored read-only; nothing outside
/// the CAS ever shares their inode, so a worktree file can be modified freely
/// without affecting the cached copy.
pub struct Cas {
    root: PathBuf,
}

impl Cas {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    fn blob_path(&self, digest: &SHA256) -> PathBuf {
        let hex = digest.to_string();
        self.root.join(&hex[..2]).join(hex)
    }

    /// Whether a blob with the given digest has been published.
    pub fn contains(&self, digest: &SHA256) -> bool {
        self.blob_path(digest).is_file()
    }

    /// Copy `src` into the store and return a reference to it. Concurrent
    /// writers of the same content converge on one blob: the first rename
    /// wins and later copies are discarded.
    pub fn put_file(&self, src: &Path) -> Result<ArtifactRef, CasError> {
        let meta = fs::metadata(src)?;
        let mode = permission_bits(&meta);

        // Hash the copy rather than the source so the digest describes the
        // bytes that were actually stored.
        let staging = self.root.join("tmp");
        fs::create_dir_all(&staging)?;
        let tmp = staging.join(format!("put.{}.{}", std::process::id(), unique_suffix()));
        let result = copy_and_hash(src, &tmp);
        let (digest, size) = match result {
            Ok(v) => v,
            Err(e) => {
                let _ = fs::remove_file(&tmp);
                return Err(e);
            }
        };

        let dest = self.blob_path(&digest);
        if dest.is_file() {
            let _ = fs::remove_file(&tmp);
            return Ok(ArtifactRef { digest, size, mode });
        }
        if let Some(parent) = dest.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::set_permissions(&tmp, fs::Permissions::from_mode(0o444))?;
        match fs::rename(&tmp, &dest) {
            Ok(()) => {}
            // Lost the race; another writer published the same digest.
            Err(_) if dest.is_file() => {
                let _ = fs::remove_file(&tmp);
            }
            Err(e) => {
                let _ = fs::remove_file(&tmp);
                return Err(e.into());
            }
        }
        Ok(ArtifactRef { digest, size, mode })
    }

    /// Recreate `artifact` at `dest` from the store. The bytes are copied to
    /// a temporary sibling, verified against the digest, given the recorded
    /// permissions, and renamed into place, so `dest` is never partially
    /// written. A blob whose content no longer matches its digest is removed
    /// and reported as [`CasError::Corrupt`] so callers treat it as a miss.
    pub fn materialize(&self, artifact: &ArtifactRef, dest: &Path) -> Result<(), CasError> {
        let blob = self.blob_path(&artifact.digest);
        if !blob.is_file() {
            return Err(CasError::Missing(artifact.digest));
        }
        let parent = match dest.parent() {
            Some(p) if !p.as_os_str().is_empty() => p.to_path_buf(),
            _ => PathBuf::from("."),
        };
        fs::create_dir_all(&parent)?;
        let name = dest
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        let tmp = parent.join(format!(".{name}.bit-tmp.{}.{}", std::process::id(), unique_suffix()));

        let result = copy_and_hash(&blob, &tmp).and_then(|(actual, _)| {
            if actual != artifact.digest {
                return Err(CasError::Corrupt {
                    expected: artifact.digest,
                    actual,
                });
            }
            fs::set_permissions(&tmp, fs::Permissions::from_mode(artifact.mode))?;
            fs::rename(&tmp, dest)?;
            Ok(())
        });
        if let Err(e) = &result {
            let _ = fs::remove_file(&tmp);
            if matches!(e, CasError::Corrupt { .. }) {
                let _ = fs::set_permissions(&blob, fs::Permissions::from_mode(0o644));
                let _ = fs::remove_file(&blob);
            }
        }
        result
    }
}

/// Copy `src` to `dst` and return the digest and size of the copy.
///
/// `fs::copy` attempts a copy-on-write clone on filesystems that support it
/// (APFS, btrfs, XFS) and falls back to a byte copy elsewhere. Either way the
/// destination is an independent file with its own inode.
fn copy_and_hash(src: &Path, dst: &Path) -> Result<(SHA256, u64), CasError> {
    let size = fs::copy(src, dst)?;
    let digest = hash_path(dst)?;
    Ok((digest, size))
}

/// Stream a file through SHA-256 without loading it into memory.
pub fn hash_path(path: &Path) -> io::Result<SHA256> {
    let mut file = fs::File::open(path)?;
    let mut hasher = Hasher::new();
    let mut buf = [0u8; 64 * 1024];
    loop {
        let n = file.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(hasher.finalize())
}

fn unique_suffix() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or_default()
}

/// Owner/group/other permission bits only; setuid/setgid/sticky bits are
/// never carried into the cache.
fn permission_bits(meta: &fs::Metadata) -> u32 {
    meta.permissions().mode() & 0o777
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_cas() -> (tempfile::TempDir, Cas) {
        let dir = tempfile::tempdir().unwrap();
        let cas = Cas::new(dir.path().join("cas"));
        (dir, cas)
    }

    #[test]
    fn put_and_materialize_roundtrip() {
        let (dir, cas) = temp_cas();
        let src = dir.path().join("bin");
        fs::write(&src, b"#!/bin/sh\necho hi\n").unwrap();
        fs::set_permissions(&src, fs::Permissions::from_mode(0o755)).unwrap();

        let artifact = cas.put_file(&src).unwrap();
        assert_eq!(artifact.digest, SHA256::digest(b"#!/bin/sh\necho hi\n"));
        assert_eq!(artifact.mode, 0o755);
        assert!(cas.contains(&artifact.digest));

        let dest = dir.path().join("out/restored");
        cas.materialize(&artifact, &dest).unwrap();
        assert_eq!(fs::read(&dest).unwrap(), b"#!/bin/sh\necho hi\n");
        assert_eq!(fs::metadata(&dest).unwrap().permissions().mode() & 0o777, 0o755);
    }

    #[test]
    fn modifying_materialized_file_does_not_affect_blob() {
        let (dir, cas) = temp_cas();
        let src = dir.path().join("src");
        fs::write(&src, b"original").unwrap();
        let artifact = cas.put_file(&src).unwrap();

        let dest = dir.path().join("dest");
        cas.materialize(&artifact, &dest).unwrap();
        fs::set_permissions(&dest, fs::Permissions::from_mode(0o644)).unwrap();
        fs::write(&dest, b"tampered").unwrap();

        let again = dir.path().join("again");
        cas.materialize(&artifact, &again).unwrap();
        assert_eq!(fs::read(&again).unwrap(), b"original");
    }

    #[test]
    fn corrupt_blob_is_reported_and_removed() {
        let (dir, cas) = temp_cas();
        let src = dir.path().join("src");
        fs::write(&src, b"good").unwrap();
        let artifact = cas.put_file(&src).unwrap();

        let blob = cas.blob_path(&artifact.digest);
        fs::set_permissions(&blob, fs::Permissions::from_mode(0o644)).unwrap();
        fs::write(&blob, b"bad!").unwrap();

        let dest = dir.path().join("dest");
        let err = cas.materialize(&artifact, &dest).unwrap_err();
        assert!(matches!(err, CasError::Corrupt { .. }), "{err}");
        assert!(!dest.exists());
        assert!(!cas.contains(&artifact.digest));
    }

    #[test]
    fn missing_blob_is_reported() {
        let (dir, cas) = temp_cas();
        let artifact = ArtifactRef {
            digest: SHA256::digest(b"nope"),
            size: 4,
            mode: 0o644,
        };
        let err = cas.materialize(&artifact, &dir.path().join("dest")).unwrap_err();
        assert!(matches!(err, CasError::Missing(_)));
    }

    #[test]
    fn same_content_converges_on_one_blob() {
        let (dir, cas) = temp_cas();
        let a = dir.path().join("a");
        let b = dir.path().join("b");
        fs::write(&a, b"same").unwrap();
        fs::write(&b, b"same").unwrap();
        let ra = cas.put_file(&a).unwrap();
        let rb = cas.put_file(&b).unwrap();
        assert_eq!(ra.digest, rb.digest);
        assert_eq!(fs::read_dir(cas.root.join("tmp")).unwrap().count(), 0);
    }

    #[test]
    fn concurrent_writers_of_same_digest() {
        let (dir, cas) = temp_cas();
        let src = dir.path().join("src");
        fs::write(&src, vec![7u8; 1 << 20]).unwrap();
        let refs: Vec<ArtifactRef> = std::thread::scope(|s| {
            let handles: Vec<_> = (0..8).map(|_| s.spawn(|| cas.put_file(&src).unwrap())).collect();
            handles.into_iter().map(|h| h.join().unwrap()).collect()
        });
        assert!(refs.iter().all(|r| r.digest == refs[0].digest));
        let dest = dir.path().join("dest");
        cas.materialize(&refs[0], &dest).unwrap();
        assert_eq!(fs::metadata(&dest).unwrap().len(), 1 << 20);
    }

    #[test]
    fn blobs_are_read_only() {
        let (dir, cas) = temp_cas();
        let src = dir.path().join("src");
        fs::write(&src, b"x").unwrap();
        let artifact = cas.put_file(&src).unwrap();
        let mode = fs::metadata(cas.blob_path(&artifact.digest))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o222, 0);
    }
}
