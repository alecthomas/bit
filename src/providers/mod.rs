use std::path::Path;

use crate::provider::BoxError;
use crate::sha256::SHA256;

pub mod docker;
pub mod exec;
pub mod go;
pub mod pnpm;
pub mod rust;

/// Compute a SHA256 content hash for a file.
pub fn hash_file(path: &Path) -> Result<SHA256, BoxError> {
    let contents = std::fs::read(path)?;
    Ok(SHA256::digest(&contents))
}
