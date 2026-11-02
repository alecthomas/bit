use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use super::backend::{Backend, BackendError};
use super::cas::Cas;
use super::receipt::ActionKey;
use crate::sha256::SHA256;

/// The cache on local disk: one JSON file of receipts per action key, and a
/// [`Cas`] of artifact blobs that providers restore files from directly.
pub struct Disk {
    receipts: PathBuf,
    cas: Cas,
}

impl Disk {
    /// The cache under `cache_dir`.
    pub fn new(cache_dir: &Path) -> Self {
        Self {
            receipts: cache_dir.join("actions").join("v2"),
            cas: Cas::new(cache_dir.join("cas").join("v1").join("sha256")),
        }
    }

    pub fn cas(&self) -> &Cas {
        &self.cas
    }

    pub fn receipts_dir(&self) -> &Path {
        &self.receipts
    }

    fn receipts_path(&self, key: &ActionKey) -> PathBuf {
        self.receipts.join(format!("{key}.json"))
    }
}

impl Backend for Disk {
    fn get_receipts(&self, key: &ActionKey) -> Result<Option<Vec<u8>>, BackendError> {
        match fs::read(self.receipts_path(key)) {
            Ok(bytes) => Ok(Some(bytes)),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    /// Written to a temporary sibling and renamed into place, so readers
    /// never see partial JSON.
    fn put_receipts(&self, key: &ActionKey, bytes: &[u8]) -> Result<(), BackendError> {
        fs::create_dir_all(&self.receipts)?;
        let tmp = self.receipts.join(format!("{key}.tmp.{}", std::process::id()));
        let written = fs::write(&tmp, bytes).and_then(|()| fs::rename(&tmp, self.receipts_path(key)));
        if written.is_err() {
            let _ = fs::remove_file(&tmp);
        }
        Ok(written?)
    }

    fn has_blob(&self, digest: &SHA256) -> Result<bool, BackendError> {
        Ok(self.cas.contains(digest))
    }

    fn get_blob(&self, digest: &SHA256, dest: &Path) -> Result<bool, BackendError> {
        match fs::copy(self.cas.blob_path(digest), dest) {
            Ok(_) => Ok(true),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(false),
            Err(e) => Err(e.into()),
        }
    }

    /// Copied into the store and checked against `digest` there.
    fn put_blob(&self, digest: &SHA256, src: &Path) -> Result<(), BackendError> {
        let tmp = self.cas.staging_file()?;
        if let Err(e) = fs::copy(src, &tmp) {
            let _ = fs::remove_file(&tmp);
            return Err(e.into());
        }
        Ok(self.cas.adopt(digest, &tmp)?)
    }
}
