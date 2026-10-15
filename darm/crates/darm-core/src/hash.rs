use crate::{DarmError, PROTOCOL, Result};
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use std::fmt;

/// A 32-byte BLAKE3 digest. Used for content ids, entry ids, block hashes and
/// Merkle nodes. Serialized as lowercase hex.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Default)]
pub struct Hash32(pub [u8; 32]);

impl Hash32 {
    pub const ZERO: Self = Self([0; 32]);

    /// Hash `data` under a named domain. Each domain derives its own BLAKE3
    /// key, so equal inputs in different domains never produce equal digests.
    pub fn digest(domain: &str, data: &[u8]) -> Self {
        let context = format!("{PROTOCOL} {domain}");
        let mut hasher = blake3::Hasher::new_derive_key(&context);
        hasher.update(data);
        Self(*hasher.finalize().as_bytes())
    }

    pub fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }

    pub fn to_hex(&self) -> String {
        hex::encode(self.0)
    }

    pub fn from_hex(value: &str) -> Result<Self> {
        Ok(Self(decode_hex32("hash", value)?))
    }

    /// Short prefix for logs and UI. Never use it as an identifier.
    pub fn short(&self) -> String {
        hex::encode(&self.0[..6])
    }
}

pub(crate) fn decode_hex32(what: &'static str, value: &str) -> Result<[u8; 32]> {
    if value.len() != 64 {
        return Err(DarmError::malformed(what, "expected 64 hex characters"));
    }
    let mut out = [0u8; 32];
    hex::decode_to_slice(value, &mut out).map_err(|e| DarmError::malformed(what, e.to_string()))?;
    Ok(out)
}

impl fmt::Display for Hash32 {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.to_hex())
    }
}

impl fmt::Debug for Hash32 {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Hash32({})", self.short())
    }
}

impl Serialize for Hash32 {
    fn serialize<S: Serializer>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.to_hex())
    }
}

impl<'de> Deserialize<'de> for Hash32 {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> std::result::Result<Self, D::Error> {
        let value = String::deserialize(deserializer)?;
        Self::from_hex(&value).map_err(serde::de::Error::custom)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn domain_separation() {
        assert_ne!(Hash32::digest("a", b"x"), Hash32::digest("b", b"x"));
        assert_eq!(Hash32::digest("a", b"x"), Hash32::digest("a", b"x"));
    }

    #[test]
    fn hex_round_trip_and_rejects_bad_input() {
        let hash = Hash32::digest("a", b"x");
        assert_eq!(Hash32::from_hex(&hash.to_hex()).unwrap(), hash);
        assert!(Hash32::from_hex("abcd").is_err());
        assert!(Hash32::from_hex(&"zz".repeat(32)).is_err());
    }
}
