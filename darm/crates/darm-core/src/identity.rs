//! Ed25519 node identities.
//!
//! A [`NodeId`] is the node's Ed25519 public key, so any record can be checked
//! with nothing but the record itself. Verification uses `verify_strict`,
//! which rejects small-order keys and non-canonical signatures (no signature
//! malleability, so a record's id cannot be changed by re-encoding its
//! signature).
//!
//! The signature type carries an algorithm tag so a post-quantum scheme
//! (ML-DSA, FIPS 204) can be added as a hybrid without a wire break.

use crate::hash::decode_hex32;
use crate::{DarmError, Result};
use ed25519_dalek::{Signer, SigningKey, VerifyingKey};
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use std::fmt;
use std::path::Path;
use zeroize::Zeroizing;

#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct NodeId(pub [u8; 32]);

impl NodeId {
    pub fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }

    pub fn to_hex(&self) -> String {
        hex::encode(self.0)
    }

    pub fn from_hex(value: &str) -> Result<Self> {
        let bytes = decode_hex32("node id", value)?;
        // Reject strings that are not valid curve points up front so a bad
        // allowlist entry fails at configuration time, not at first use.
        VerifyingKey::from_bytes(&bytes).map_err(|_| DarmError::InvalidPublicKey)?;
        Ok(Self(bytes))
    }

    pub fn short(&self) -> String {
        hex::encode(&self.0[..6])
    }

    /// Verify `signature` over `message` (already canonically encoded and
    /// domain separated by the caller).
    pub fn verify(&self, message: &[u8], signature: &Signature) -> Result<()> {
        let Signature::Ed25519(bytes) = signature;
        let key = VerifyingKey::from_bytes(&self.0).map_err(|_| DarmError::InvalidPublicKey)?;
        let signature = ed25519_dalek::Signature::from_bytes(bytes);
        key.verify_strict(message, &signature)
            .map_err(|_| DarmError::InvalidSignature)
    }
}

impl fmt::Display for NodeId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.to_hex())
    }
}

impl fmt::Debug for NodeId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "NodeId({})", self.short())
    }
}

impl Serialize for NodeId {
    fn serialize<S: Serializer>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.to_hex())
    }
}

impl<'de> Deserialize<'de> for NodeId {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> std::result::Result<Self, D::Error> {
        let value = String::deserialize(deserializer)?;
        Self::from_hex(&value).map_err(serde::de::Error::custom)
    }
}

/// Algorithm-tagged signature. Serialized as `"ed25519:<hex>"`.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Signature {
    Ed25519([u8; 64]),
}

impl Signature {
    pub fn to_tagged_string(&self) -> String {
        match self {
            Self::Ed25519(bytes) => format!("ed25519:{}", hex::encode(bytes)),
        }
    }

    pub fn from_tagged_str(value: &str) -> Result<Self> {
        let Some(hex_part) = value.strip_prefix("ed25519:") else {
            return Err(DarmError::malformed(
                "signature",
                "unsupported algorithm tag",
            ));
        };
        if hex_part.len() != 128 {
            return Err(DarmError::malformed(
                "signature",
                "expected 128 hex characters",
            ));
        }
        let mut bytes = [0u8; 64];
        hex::decode_to_slice(hex_part, &mut bytes)
            .map_err(|e| DarmError::malformed("signature", e.to_string()))?;
        Ok(Self::Ed25519(bytes))
    }
}

impl fmt::Debug for Signature {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Ed25519(bytes) => write!(f, "Signature(ed25519:{})", hex::encode(&bytes[..6])),
        }
    }
}

impl Serialize for Signature {
    fn serialize<S: Serializer>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.to_tagged_string())
    }
}

impl<'de> Deserialize<'de> for Signature {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> std::result::Result<Self, D::Error> {
        let value = String::deserialize(deserializer)?;
        Self::from_tagged_str(&value).map_err(serde::de::Error::custom)
    }
}

/// A node's private signing identity. The secret is zeroized on drop and is
/// never printed by `Debug`.
pub struct NodeIdentity {
    key: SigningKey,
}

impl NodeIdentity {
    /// Fresh identity from the operating system CSPRNG.
    pub fn generate() -> Result<Self> {
        let mut seed = Zeroizing::new([0u8; 32]);
        getrandom::fill(seed.as_mut()).map_err(|e| DarmError::Entropy(e.to_string()))?;
        Ok(Self::from_secret_bytes(&seed))
    }

    pub fn from_secret_bytes(secret: &[u8; 32]) -> Self {
        Self {
            key: SigningKey::from_bytes(secret),
        }
    }

    pub fn node_id(&self) -> NodeId {
        NodeId(self.key.verifying_key().to_bytes())
    }

    pub fn sign(&self, message: &[u8]) -> Signature {
        Signature::Ed25519(self.key.sign(message).to_bytes())
    }

    /// Load the identity stored at `path`, creating it (owner-only
    /// permissions on Unix) when absent. An existing key file that other
    /// users can read is refused rather than silently used.
    pub fn load_or_create(path: &Path) -> Result<Self> {
        match std::fs::read_to_string(path) {
            Ok(contents) => {
                check_private_permissions(path)?;
                let contents = Zeroizing::new(contents);
                let secret = Zeroizing::new(decode_hex32("identity key", contents.trim())?);
                Ok(Self::from_secret_bytes(&secret))
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                let identity = Self::generate()?;
                if let Some(parent) = path.parent() {
                    std::fs::create_dir_all(parent)?;
                }
                let encoded = Zeroizing::new(hex::encode(identity.key.to_bytes()));
                write_private_file(path, encoded.as_bytes())?;
                Ok(identity)
            }
            Err(error) => Err(error.into()),
        }
    }
}

impl fmt::Debug for NodeIdentity {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("NodeIdentity")
            .field("node_id", &self.node_id())
            .field("secret", &"<redacted>")
            .finish()
    }
}

#[cfg(unix)]
fn write_private_file(path: &Path, contents: &[u8]) -> Result<()> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)?;
    file.write_all(contents)?;
    file.sync_all()?;
    Ok(())
}

#[cfg(not(unix))]
fn write_private_file(path: &Path, contents: &[u8]) -> Result<()> {
    use std::io::Write;
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)?;
    file.write_all(contents)?;
    file.sync_all()?;
    Ok(())
}

#[cfg(unix)]
fn check_private_permissions(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let mode = std::fs::metadata(path)?.permissions().mode();
    if mode & 0o077 != 0 {
        return Err(DarmError::Unauthorized(format!(
            "identity key {} is accessible by other users (mode {:o}); run chmod 600",
            path.display(),
            mode & 0o777
        )));
    }
    Ok(())
}

#[cfg(not(unix))]
fn check_private_permissions(_path: &Path) -> Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sign_and_verify() {
        let identity = NodeIdentity::from_secret_bytes(&[7; 32]);
        let signature = identity.sign(b"hello");
        identity.node_id().verify(b"hello", &signature).unwrap();
        assert_eq!(
            identity.node_id().verify(b"hellO", &signature),
            Err(DarmError::InvalidSignature)
        );
        let other = NodeIdentity::from_secret_bytes(&[8; 32]);
        assert_eq!(
            other.node_id().verify(b"hello", &signature),
            Err(DarmError::InvalidSignature),
            "a signature must not verify under another node's key"
        );
    }

    #[test]
    fn serde_round_trip() {
        let identity = NodeIdentity::from_secret_bytes(&[7; 32]);
        let signature = identity.sign(b"x");
        let json = serde_json::to_string(&(identity.node_id(), signature)).unwrap();
        let (node, sig): (NodeId, Signature) = serde_json::from_str(&json).unwrap();
        assert_eq!(node, identity.node_id());
        assert_eq!(sig, signature);
        assert!(serde_json::from_str::<Signature>("\"rsa:00\"").is_err());
    }

    #[test]
    fn debug_never_prints_secret() {
        let identity = NodeIdentity::from_secret_bytes(&[0xAB; 32]);
        let rendered = format!("{identity:?}");
        assert!(rendered.contains("redacted"));
        assert!(!rendered.contains("abababab"));
    }

    #[test]
    fn load_or_create_persists_and_checks_permissions() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("keys/node.key");
        let first = NodeIdentity::load_or_create(&path).unwrap();
        let second = NodeIdentity::load_or_create(&path).unwrap();
        assert_eq!(first.node_id(), second.node_id());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
            assert!(matches!(
                NodeIdentity::load_or_create(&path),
                Err(DarmError::Unauthorized(_))
            ));
        }
    }
}
