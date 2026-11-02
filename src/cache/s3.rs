use std::path::Path;
use std::sync::{Arc, OnceLock};

use super::backend::{Backend, BackendError};
use super::receipt::ActionKey;
use crate::sha256::SHA256;

/// A cache in an S3 bucket, or any S3-compatible service, laid out as the
/// disk cache is: `actions/v2/<key>.json` and `cas/v1/sha256/<ab>/<digest>`
/// under a key prefix.
///
/// Credentials, region, and endpoint come from the usual AWS environment
/// variables and configuration files; see [`bit_s3::Config::from_env`].
/// They are resolved on first use, so an operation that needs none, such as
/// refusing to clean, does not fail for want of them, and a run without
/// them reports it as any other remote failure.
pub struct S3 {
    /// The resolved client, or why it could not be resolved. Kept either way
    /// so a missing credential is looked for, and reported, once.
    client: OnceLock<Result<bit_s3::Client, Arc<bit_s3::Error>>>,
    bucket: String,
    /// Empty, or ending in `/`.
    prefix: String,
}

impl S3 {
    pub fn new(bucket: &str, prefix: &str) -> Self {
        Self {
            client: OnceLock::new(),
            bucket: bucket.to_owned(),
            prefix: if prefix.is_empty() {
                String::new()
            } else {
                format!("{prefix}/")
            },
        }
    }

    fn client(&self) -> Result<&bit_s3::Client, BackendError> {
        self.client
            .get_or_init(|| {
                bit_s3::Config::from_env()
                    .and_then(bit_s3::Client::new)
                    .map_err(Arc::new)
            })
            .as_ref()
            .map_err(|e| BackendError::Remote(Box::new(e.clone())))
    }

    fn receipts_key(&self, key: &ActionKey) -> String {
        format!("{}actions/v2/{key}.json", self.prefix)
    }

    fn blob_key(&self, digest: &SHA256) -> String {
        let hex = digest.to_string();
        format!("{}cas/v1/sha256/{}/{hex}", self.prefix, &hex[..2])
    }
}

fn remote(e: bit_s3::Error) -> BackendError {
    BackendError::Remote(Box::new(e))
}

/// S3 keeps the default [`Backend::clean`], which refuses.
impl Backend for S3 {
    fn get_receipts(&self, key: &ActionKey) -> Result<Option<Vec<u8>>, BackendError> {
        self.client()?
            .get(&self.bucket, &self.receipts_key(key))
            .map_err(remote)
    }

    fn put_receipts(&self, key: &ActionKey, bytes: &[u8]) -> Result<(), BackendError> {
        self.client()?
            .put(&self.bucket, &self.receipts_key(key), bytes)
            .map_err(remote)
    }

    fn has_blob(&self, digest: &SHA256) -> Result<bool, BackendError> {
        self.client()?
            .head(&self.bucket, &self.blob_key(digest))
            .map_err(remote)
    }

    fn get_blob(&self, digest: &SHA256, dest: &Path) -> Result<bool, BackendError> {
        self.client()?
            .get_to_file(&self.bucket, &self.blob_key(digest), dest)
            .map_err(remote)
    }

    fn put_blob(&self, digest: &SHA256, src: &Path) -> Result<(), BackendError> {
        self.client()?
            .put_file(&self.bucket, &self.blob_key(digest), src)
            .map_err(remote)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn refuses_to_clean_without_resolving_credentials() {
        let s3 = S3::new("bucket", "prefix");
        assert!(matches!(s3.clean(), Err(BackendError::CannotClean)));
        assert!(s3.client.get().is_none(), "no credentials were looked for");
    }

    #[test]
    fn keys_follow_the_disk_layout_under_the_prefix() {
        let digest = SHA256::digest(b"x");
        let hex = digest.to_string();
        let s3 = S3::new("bucket", "team/bit");
        assert_eq!(
            s3.blob_key(&digest),
            format!("team/bit/cas/v1/sha256/{}/{hex}", &hex[..2])
        );
        assert_eq!(
            S3::new("bucket", "").blob_key(&digest),
            format!("cas/v1/sha256/{}/{hex}", &hex[..2])
        );
    }
}
