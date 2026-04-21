use std::path::Path;

use sha2::Digest;

use crate::provider::BoxError;

pub mod docker;
pub mod exec;
pub mod go;
pub mod pnpm;
pub mod rust;

/// Compute a SHA256 content hash for a file.
pub fn hash_file(path: &Path) -> Result<String, BoxError> {
    let contents = std::fs::read(path)?;
    let mut hasher = sha2::Sha256::new();
    hasher.update(&contents);
    Ok(format!("{:x}", hasher.finalize()))
}
