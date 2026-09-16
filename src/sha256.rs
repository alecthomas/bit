use std::fmt;

use serde::{Deserialize, Deserializer, Serialize, Serializer};
use sha2::Digest;

/// Compact 32-byte SHA256 hash.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct SHA256([u8; 32]);

impl SHA256 {
    pub const ZERO: SHA256 = SHA256([0; 32]);

    /// Compute the SHA256 hash of a byte slice.
    pub fn digest(data: &[u8]) -> Self {
        Self(sha2::Sha256::digest(data).into())
    }

    /// Wrap raw bytes into a SHA256.
    pub fn from_bytes(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }

    pub fn is_zero(&self) -> bool {
        self == &Self::ZERO
    }
}

impl From<sha2::digest::Output<sha2::Sha256>> for SHA256 {
    fn from(output: sha2::digest::Output<sha2::Sha256>) -> Self {
        Self(output.into())
    }
}

impl fmt::Display for SHA256 {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for byte in &self.0 {
            write!(f, "{byte:02x}")?;
        }
        Ok(())
    }
}

impl fmt::Debug for SHA256 {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "SHA256({self})")
    }
}

impl Default for SHA256 {
    fn default() -> Self {
        Self::ZERO
    }
}

impl Serialize for SHA256 {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.to_string())
    }
}

impl<'de> Deserialize<'de> for SHA256 {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let s = String::deserialize(deserializer)?;
        parse_hex(&s).map_err(serde::de::Error::custom)
    }
}

fn parse_hex(s: &str) -> Result<SHA256, String> {
    if s.is_empty() {
        return Ok(SHA256::ZERO);
    }
    if s.len() != 64 {
        return Err(format!("expected 64 hex chars, got {}", s.len()));
    }
    let mut bytes = [0u8; 32];
    for (i, chunk) in s.as_bytes().chunks(2).enumerate() {
        let hi = hex_digit(chunk[0])?;
        let lo = hex_digit(chunk[1])?;
        bytes[i] = (hi << 4) | lo;
    }
    Ok(SHA256(bytes))
}

fn hex_digit(b: u8) -> Result<u8, String> {
    match b {
        b'0'..=b'9' => Ok(b - b'0'),
        b'a'..=b'f' => Ok(b - b'a' + 10),
        b'A'..=b'F' => Ok(b - b'A' + 10),
        _ => Err(format!("invalid hex digit: {}", b as char)),
    }
}

/// Incremental hasher that produces a SHA256.
pub struct Hasher(sha2::Sha256);

impl Hasher {
    pub fn new() -> Self {
        Self(sha2::Sha256::new())
    }

    pub fn update(&mut self, data: &[u8]) {
        self.0.update(data);
    }

    pub fn finalize(self) -> SHA256 {
        self.0.finalize().into()
    }
}

impl Default for Hasher {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn digest() {
        let hash = SHA256::digest(b"hello");
        assert_eq!(
            hash.to_string(),
            "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824"
        );
    }

    #[test]
    fn zero() {
        assert!(SHA256::ZERO.is_zero());
        assert!(!SHA256::digest(b"hello").is_zero());
    }

    #[test]
    fn display_debug() {
        let hash = SHA256::digest(b"test");
        let display = format!("{hash}");
        let debug = format!("{hash:?}");
        assert_eq!(display.len(), 64);
        assert!(debug.starts_with("SHA256("));
    }

    #[test]
    fn serde_roundtrip() {
        let hash = SHA256::digest(b"hello");
        let json = serde_json::to_string(&hash).unwrap();
        let parsed: SHA256 = serde_json::from_str(&json).unwrap();
        assert_eq!(hash, parsed);
    }

    #[test]
    fn deserialize_empty_string() {
        let parsed: SHA256 = serde_json::from_str("\"\"").unwrap();
        assert_eq!(parsed, SHA256::ZERO);
    }

    #[test]
    fn hasher_matches_digest() {
        let mut h = Hasher::new();
        h.update(b"hello");
        assert_eq!(h.finalize(), SHA256::digest(b"hello"));
    }

    #[test]
    fn hasher_incremental() {
        let mut h = Hasher::new();
        h.update(b"hel");
        h.update(b"lo");
        assert_eq!(h.finalize(), SHA256::digest(b"hello"));
    }
}
