use std::fs;
use std::io::{self, Read};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};

use serde::{Deserialize, Serialize};

use crate::sha256::{Hasher, SHA256};

static NEXT_TEMP_ID: AtomicUsize = AtomicUsize::new(0);

#[derive(Debug, thiserror::Error)]
pub enum CasError {
    #[error("io error: {0}")]
    Io(#[from] io::Error),
    #[error("artifact {0} is not in the cache")]
    Missing(SHA256),
    #[error("artifact {expected} is corrupt (content hashes to {actual})")]
    Corrupt { expected: SHA256, actual: SHA256 },
    #[error("{0}")]
    Unsupported(String),
    #[error("tree manifest is malformed: {0}")]
    Manifest(#[from] serde_json::Error),
}

/// A durable artifact stored in the CAS, as referenced by an action receipt.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum ArtifactRef {
    /// The content of one file.
    File(FileRef),
    /// A directory tree, recorded as a manifest blob naming every entry
    /// beneath it. The manifest is itself content-addressed, so two captures
    /// of the same tree converge on one blob.
    Tree { manifest: SHA256, mode: u32 },
}

impl ArtifactRef {
    /// The file this artifact records, or `None` when it records a tree.
    pub fn file(&self) -> Option<&FileRef> {
        match self {
            ArtifactRef::File(file) => Some(file),
            ArtifactRef::Tree { .. } => None,
        }
    }
}

/// One file's content and permissions.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileRef {
    pub digest: SHA256,
    pub size: u64,
    /// Unix permission bits to apply when the artifact is materialized.
    pub mode: u32,
}

impl FileRef {
    /// Whether the file at `path` already has this content and executable
    /// bits, i.e. materializing it would be a no-op.
    pub fn matches(&self, path: &Path) -> bool {
        let Ok(meta) = fs::metadata(path) else {
            return false;
        };
        if !meta.is_file() || meta.len() != self.size || meta.permissions().mode() & 0o111 != self.mode & 0o111 {
            return false;
        }
        hash_path(path).is_ok_and(|digest| digest == self.digest)
    }
}

/// One entry of a captured directory tree, at a path relative to its root.
///
/// Symlinks and other irregular entries have no representation here: a tree
/// containing one is rejected at capture rather than restored as something
/// it is not.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
enum TreeEntry {
    Dir {
        path: String,
        mode: u32,
    },
    File {
        path: String,
        #[serde(flatten)]
        file: FileRef,
    },
}

impl TreeEntry {
    fn path(&self) -> &str {
        match self {
            TreeEntry::Dir { path, .. } | TreeEntry::File { path, .. } => path,
        }
    }
}

/// The content of a tree manifest blob. Entries are sorted by path, so a
/// parent directory always precedes what it contains and the serialized form
/// is identical for identical trees.
#[derive(Debug, Serialize, Deserialize)]
struct TreeManifest {
    entries: Vec<TreeEntry>,
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
        Ok(ArtifactRef::File(self.put_file_ref(src)?))
    }

    fn put_file_ref(&self, src: &Path) -> Result<FileRef, CasError> {
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
            return Ok(FileRef { digest, size, mode });
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
        Ok(FileRef { digest, size, mode })
    }

    /// Capture the directory at `src` as a tree: every file beneath it
    /// becomes a blob, and the listing of the whole tree becomes one more.
    ///
    /// A symlink or other irregular entry is an error rather than a silent
    /// omission, because restoring the tree without it would produce
    /// something the action never built.
    pub fn put_tree(&self, src: &Path) -> Result<ArtifactRef, CasError> {
        let mut entries = Vec::new();
        self.collect_tree(src, Path::new(""), &mut entries)?;
        entries.sort_by(|a, b| a.path().cmp(b.path()));

        let json = serde_json::to_vec(&TreeManifest { entries })?;
        let staging = self.root.join("tmp");
        fs::create_dir_all(&staging)?;
        let tmp = staging.join(format!("tree.{}.{}", std::process::id(), unique_suffix()));
        fs::write(&tmp, &json)?;
        let manifest = self.put_file_ref(&tmp);
        let _ = fs::remove_file(&tmp);

        Ok(ArtifactRef::Tree {
            manifest: manifest?.digest,
            mode: permission_bits(&fs::metadata(src)?),
        })
    }

    fn collect_tree(&self, root: &Path, relative: &Path, entries: &mut Vec<TreeEntry>) -> Result<(), CasError> {
        for entry in fs::read_dir(root.join(relative))? {
            let entry = entry?;
            let child = relative.join(entry.file_name());
            let path = child
                .to_str()
                .ok_or_else(|| CasError::Unsupported("captured directory holds a non-UTF-8 path".into()))?
                .to_owned();
            // `file_type` does not follow symlinks, so a link is reported as
            // a link rather than as whatever it points at.
            let file_type = entry.file_type()?;
            if file_type.is_dir() {
                entries.push(TreeEntry::Dir {
                    path,
                    mode: permission_bits(&entry.metadata()?),
                });
                self.collect_tree(root, &child, entries)?;
            } else if file_type.is_file() {
                entries.push(TreeEntry::File {
                    path,
                    file: self.put_file_ref(&root.join(&child))?,
                });
            } else {
                return Err(CasError::Unsupported(format!(
                    "{path} is not a regular file or directory and cannot be cached"
                )));
            }
        }
        Ok(())
    }

    fn read_manifest(&self, digest: &SHA256) -> Result<TreeManifest, CasError> {
        let blob = self.blob_path(digest);
        if !blob.is_file() {
            return Err(CasError::Missing(*digest));
        }
        let actual = hash_path(&blob)?;
        if actual != *digest {
            let _ = fs::set_permissions(&blob, fs::Permissions::from_mode(0o644));
            let _ = fs::remove_file(&blob);
            return Err(CasError::Corrupt {
                expected: *digest,
                actual,
            });
        }
        let bytes = fs::read(&blob)?;
        Ok(serde_json::from_slice(&bytes)?)
    }

    /// Whether `path` already holds this artifact, i.e. materializing it
    /// would be a no-op. A tree matches only when it has exactly the recorded
    /// entries: a file the tree does not contain makes it stale, because
    /// restoring is defined to leave the destination equal to what was
    /// captured.
    pub fn matches(&self, artifact: &ArtifactRef, path: &Path) -> bool {
        match artifact {
            ArtifactRef::File(file) => file.matches(path),
            ArtifactRef::Tree { manifest, .. } => {
                let Ok(manifest) = self.read_manifest(manifest) else {
                    return false;
                };
                let mut present = Vec::new();
                if list_tree(path, Path::new(""), &mut present).is_err() {
                    return false;
                }
                present.sort();
                let recorded: Vec<&str> = manifest.entries.iter().map(TreeEntry::path).collect();
                if present != recorded {
                    return false;
                }
                manifest.entries.iter().all(|entry| match entry {
                    TreeEntry::Dir { .. } => true,
                    TreeEntry::File { path: rel, file } => file.matches(&path.join(rel)),
                })
            }
        }
    }

    /// Recreate `artifact` at `dest` from the store.
    ///
    /// A file is copied to a temporary sibling, verified against the digest,
    /// given the recorded permissions, and renamed into place, so `dest` is
    /// never partially written. A tree is assembled the same way, entry by
    /// entry, and swapped in whole; anything already at `dest` is replaced,
    /// so the result is exactly the tree that was captured. A blob whose
    /// content no longer matches its digest is removed and reported as
    /// [`CasError::Corrupt`] so callers treat it as a miss.
    pub fn materialize(&self, artifact: &ArtifactRef, dest: &Path) -> Result<(), CasError> {
        match artifact {
            ArtifactRef::File(file) => self.materialize_file(file, dest),
            ArtifactRef::Tree { manifest, mode } => self.materialize_tree(manifest, *mode, dest),
        }
    }

    /// Assemble a tree beside `dest` and swap it in, so a failure part-way
    /// through leaves the existing directory untouched.
    fn materialize_tree(&self, manifest: &SHA256, mode: u32, dest: &Path) -> Result<(), CasError> {
        let manifest = self.read_manifest(manifest)?;
        let staging = staging_path(dest);
        let _ = fs::remove_dir_all(&staging);
        fs::create_dir_all(&staging)?;

        let build = || -> Result<(), CasError> {
            // Entries are sorted by path, so a directory is always created
            // before anything it contains.
            for entry in &manifest.entries {
                match entry {
                    TreeEntry::Dir { path, mode } => {
                        let dir = staging.join(path);
                        fs::create_dir_all(&dir)?;
                        fs::set_permissions(&dir, fs::Permissions::from_mode(*mode))?;
                    }
                    TreeEntry::File { path, file } => self.materialize_file(file, &staging.join(path))?,
                }
            }
            fs::set_permissions(&staging, fs::Permissions::from_mode(mode))?;
            Ok(())
        };
        if let Err(e) = build() {
            let _ = fs::remove_dir_all(&staging);
            return Err(e);
        }

        if dest.is_dir() {
            fs::remove_dir_all(dest)?;
        } else if dest.exists() {
            fs::remove_file(dest)?;
        }
        if let Some(parent) = dest.parent()
            && !parent.as_os_str().is_empty()
        {
            fs::create_dir_all(parent)?;
        }
        if let Err(e) = fs::rename(&staging, dest) {
            let _ = fs::remove_dir_all(&staging);
            return Err(e.into());
        }
        Ok(())
    }

    fn materialize_file(&self, artifact: &FileRef, dest: &Path) -> Result<(), CasError> {
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

/// A sibling of `dest` to assemble a tree in before swapping it into place.
fn staging_path(dest: &Path) -> PathBuf {
    let parent = match dest.parent() {
        Some(p) if !p.as_os_str().is_empty() => p.to_path_buf(),
        _ => PathBuf::from("."),
    };
    let name = dest
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    parent.join(format!(".{name}.bit-tmp.{}.{}", std::process::id(), unique_suffix()))
}

/// Collect every path beneath `root`, relative to it, for comparison against
/// a manifest. Irregular entries are listed like anything else, so a tree
/// that grew a symlink no longer matches.
fn list_tree(root: &Path, relative: &Path, out: &mut Vec<String>) -> io::Result<()> {
    for entry in fs::read_dir(root.join(relative))? {
        let entry = entry?;
        let child = relative.join(entry.file_name());
        out.push(child.to_string_lossy().into_owned());
        if entry.file_type()?.is_dir() {
            list_tree(root, &child, out)?;
        }
    }
    Ok(())
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

fn unique_suffix() -> usize {
    // The PID separates processes. Within one process, the atomic value is
    // used only as an identity, so relaxed ordering is sufficient.
    NEXT_TEMP_ID.fetch_add(1, Ordering::Relaxed)
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
        let file = artifact.file().unwrap();
        assert_eq!(file.digest, SHA256::digest(b"#!/bin/sh\necho hi\n"));
        assert_eq!(file.mode, 0o755);
        assert!(cas.contains(&file.digest));

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

        let blob = cas.blob_path(&artifact.file().unwrap().digest);
        fs::set_permissions(&blob, fs::Permissions::from_mode(0o644)).unwrap();
        fs::write(&blob, b"bad!").unwrap();

        let dest = dir.path().join("dest");
        let err = cas.materialize(&artifact, &dest).unwrap_err();
        assert!(matches!(err, CasError::Corrupt { .. }), "{err}");
        assert!(!dest.exists());
        assert!(!cas.contains(&artifact.file().unwrap().digest));
    }

    /// Build a small tree: a file, an empty directory, and a nested file.
    fn write_tree(root: &Path) {
        fs::create_dir_all(root.join("assets")).unwrap();
        fs::create_dir_all(root.join("empty")).unwrap();
        fs::write(root.join("index.js"), b"main").unwrap();
        fs::write(root.join("assets/run"), b"#!/bin/sh\n").unwrap();
        fs::set_permissions(root.join("assets/run"), fs::Permissions::from_mode(0o755)).unwrap();
    }

    #[test]
    fn put_and_materialize_tree_roundtrip() {
        let (dir, cas) = temp_cas();
        let src = dir.path().join("dist");
        write_tree(&src);

        let artifact = cas.put_tree(&src).unwrap();
        assert!(cas.matches(&artifact, &src));

        let dest = dir.path().join("out/dist");
        cas.materialize(&artifact, &dest).unwrap();
        assert_eq!(fs::read(dest.join("index.js")).unwrap(), b"main");
        assert!(dest.join("empty").is_dir(), "empty directories are preserved");
        assert_eq!(
            fs::metadata(dest.join("assets/run")).unwrap().permissions().mode() & 0o777,
            0o755
        );
        assert!(cas.matches(&artifact, &dest));
    }

    #[test]
    fn identical_trees_converge_on_one_manifest() {
        let (dir, cas) = temp_cas();
        write_tree(&dir.path().join("a"));
        write_tree(&dir.path().join("b"));

        let a = cas.put_tree(&dir.path().join("a")).unwrap();
        let b = cas.put_tree(&dir.path().join("b")).unwrap();

        assert_eq!(a, b);
    }

    #[test]
    fn a_tree_with_an_extra_or_changed_file_does_not_match() {
        let (dir, cas) = temp_cas();
        let src = dir.path().join("dist");
        write_tree(&src);
        let artifact = cas.put_tree(&src).unwrap();

        fs::write(src.join("stray.txt"), b"left over").unwrap();
        assert!(!cas.matches(&artifact, &src), "an unrecorded file makes the tree stale");

        fs::remove_file(src.join("stray.txt")).unwrap();
        fs::write(src.join("index.js"), b"edited").unwrap();
        assert!(!cas.matches(&artifact, &src));
    }

    /// Restoring is defined to leave the destination equal to what was
    /// captured, so anything already there is replaced rather than merged.
    #[test]
    fn materializing_a_tree_replaces_the_destination() {
        let (dir, cas) = temp_cas();
        let src = dir.path().join("dist");
        write_tree(&src);
        let artifact = cas.put_tree(&src).unwrap();

        let dest = dir.path().join("other");
        fs::create_dir_all(&dest).unwrap();
        fs::write(dest.join("stale.txt"), b"old").unwrap();
        cas.materialize(&artifact, &dest).unwrap();

        assert!(!dest.join("stale.txt").exists());
        assert!(dest.join("index.js").is_file());
    }

    #[test]
    fn a_tree_containing_a_symlink_is_rejected() {
        let (dir, cas) = temp_cas();
        let src = dir.path().join("dist");
        fs::create_dir_all(&src).unwrap();
        fs::write(src.join("real"), b"x").unwrap();
        std::os::unix::fs::symlink(src.join("real"), src.join("link")).unwrap();

        let err = cas.put_tree(&src).unwrap_err();

        assert!(err.to_string().contains("not a regular file or directory"), "{err}");
    }

    #[test]
    fn missing_blob_is_reported() {
        let (dir, cas) = temp_cas();
        let artifact = ArtifactRef::File(FileRef {
            digest: SHA256::digest(b"nope"),
            size: 4,
            mode: 0o644,
        });
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
        assert_eq!(ra.file().unwrap().digest, rb.file().unwrap().digest);
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
        assert!(
            refs.iter()
                .all(|r| r.file().unwrap().digest == refs[0].file().unwrap().digest)
        );
        let dest = dir.path().join("dest");
        cas.materialize(&refs[0], &dest).unwrap();
        assert_eq!(fs::metadata(&dest).unwrap().len(), 1 << 20);
    }

    #[test]
    fn artifact_matches_checks_content_and_mode() {
        let (dir, cas) = temp_cas();
        let src = dir.path().join("src");
        fs::write(&src, b"content").unwrap();
        fs::set_permissions(&src, fs::Permissions::from_mode(0o755)).unwrap();
        let artifact = cas.put_file(&src).unwrap();
        assert!(cas.matches(&artifact, &src));

        fs::set_permissions(&src, fs::Permissions::from_mode(0o644)).unwrap();
        assert!(!cas.matches(&artifact, &src), "lost executable bit");

        fs::set_permissions(&src, fs::Permissions::from_mode(0o755)).unwrap();
        fs::write(&src, b"changed").unwrap();
        assert!(!cas.matches(&artifact, &src));
        assert!(!cas.matches(&artifact, &dir.path().join("missing")));
    }

    #[test]
    fn blobs_are_read_only() {
        let (dir, cas) = temp_cas();
        let src = dir.path().join("src");
        fs::write(&src, b"x").unwrap();
        let artifact = cas.put_file(&src).unwrap();
        let mode = fs::metadata(cas.blob_path(&artifact.file().unwrap().digest))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o222, 0);
    }
}
