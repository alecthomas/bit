use std::io;
use std::path::Path;

use super::cas::CasError;
use super::receipt::ActionKey;
use crate::sha256::SHA256;

#[derive(Debug, thiserror::Error)]
pub enum BackendError {
    #[error("io error: {0}")]
    Io(#[from] io::Error),
    #[error(transparent)]
    Cas(#[from] CasError),
    #[error("{0}")]
    Remote(Box<dyn std::error::Error + Send + Sync>),
}

/// Where cache objects are stored: the receipt set published under each
/// action key, and artifact blobs by content digest.
///
/// The local disk cache is one backend and is always present, because
/// providers restore artifacts from local files. A remote backend sits
/// behind it: receipts and blobs found there are copied to disk before use,
/// and results published locally are copied to it when it is writable.
///
/// A backend stores bytes only. Choosing among receipt variants and
/// checking that a blob matches its digest happen above it, so a backend
/// needs no knowledge of either.
pub trait Backend: Send + Sync {
    /// The serialized receipt set for `key`, or `None` if there is none.
    fn get_receipts(&self, key: &ActionKey) -> Result<Option<Vec<u8>>, BackendError>;

    /// Replace the receipt set for `key`. Readers must never observe a
    /// partial write.
    fn put_receipts(&self, key: &ActionKey, bytes: &[u8]) -> Result<(), BackendError>;

    fn has_blob(&self, digest: &SHA256) -> Result<bool, BackendError>;

    /// Write the blob for `digest` to `dest`, returning `false` if there is
    /// no such blob.
    fn get_blob(&self, digest: &SHA256, dest: &Path) -> Result<bool, BackendError>;

    /// Store the file at `src` as the blob for `digest`.
    fn put_blob(&self, digest: &SHA256, src: &Path) -> Result<(), BackendError>;
}
