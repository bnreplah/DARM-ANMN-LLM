//! Post-quantum signatures (ML-DSA-65, FIPS 204) alongside Ed25519.
//!
//! A node id is an Ed25519 public key. A large quantum computer could forge
//! Ed25519 signatures, so a node may also hold an ML-DSA-65 key and bind it
//! to its node id on the ledger (`EntryBody::PqKeyBind`). Where a hybrid
//! signature is required, both signatures must verify: forging one needs to
//! break both algorithms.
//!
//! The ML-DSA key is derived from the identity's secret seed, so there is no
//! second key file and no way for the two keys to drift apart. Breaking
//! Ed25519 reveals the private scalar, not the seed it was hashed from, so it
//! does not reveal the ML-DSA key.
//!
//! Signing uses FIPS 204's deterministic variant. The `ml-dsa` crate has not
//! been independently audited.

use crate::canonical::Canonical;
use crate::{DarmError, Hash32, Result};
use ml_dsa::signature::{Signer, Verifier};
use ml_dsa::{EncodedVerifyingKey, KeyInit, MlDsa65, Signature, SigningKey, VerifyingKey};
use zeroize::Zeroizing;

pub const PQ_PUBLIC_KEY_BYTES: usize = 1952;
pub const PQ_SIGNATURE_BYTES: usize = 3309;

/// An ML-DSA-65 public key.
#[derive(Clone)]
pub struct PqPublicKey {
    key: Box<VerifyingKey<MlDsa65>>,
    bytes: Vec<u8>,
}

impl PqPublicKey {
    pub fn from_bytes(bytes: &[u8]) -> Result<Self> {
        let encoded = EncodedVerifyingKey::<MlDsa65>::try_from(bytes)
            .map_err(|_| DarmError::malformed("pq key", "ML-DSA-65 keys are 1952 bytes"))?;
        let key = VerifyingKey::<MlDsa65>::decode(&encoded);
        // Decoding masks out-of-range coefficients; only the canonical
        // encoding is accepted, so each key has one byte string.
        if key.encode().as_slice() != bytes {
            return Err(DarmError::malformed("pq key", "not a canonical encoding"));
        }
        Ok(Self {
            key: Box::new(key),
            bytes: bytes.to_vec(),
        })
    }

    pub fn as_bytes(&self) -> &[u8] {
        &self.bytes
    }

    /// Short commitment to the key.
    pub fn fingerprint(&self) -> Hash32 {
        Hash32::digest("pq-key", &self.bytes)
    }

    /// Verify `signature` over `message` under `domain`.
    pub fn verify(&self, domain: &str, message: &[u8], signature: &[u8]) -> Result<()> {
        if signature.len() != PQ_SIGNATURE_BYTES {
            return Err(DarmError::InvalidSignature);
        }
        let signature =
            Signature::<MlDsa65>::try_from(signature).map_err(|_| DarmError::InvalidSignature)?;
        self.key
            .verify(&framed(domain, message), &signature)
            .map_err(|_| DarmError::InvalidSignature)
    }
}

impl PartialEq for PqPublicKey {
    fn eq(&self, other: &Self) -> bool {
        self.bytes == other.bytes
    }
}

impl Eq for PqPublicKey {}

impl std::fmt::Debug for PqPublicKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "PqPublicKey({})", self.fingerprint().short())
    }
}

/// Domain-separated message: no ML-DSA signature made for one purpose can be
/// presented for another.
fn framed(domain: &str, message: &[u8]) -> Vec<u8> {
    let mut encoder = Canonical::new(domain);
    encoder.bytes(message);
    encoder.as_bytes().to_vec()
}

/// An ML-DSA-65 signing key derived from a node identity.
pub struct PqIdentity {
    key: SigningKey<MlDsa65>,
    public: PqPublicKey,
}

impl PqIdentity {
    /// Derive from the identity's 32-byte secret seed.
    pub(crate) fn derive(identity_seed: &[u8; 32]) -> Self {
        let seed = Zeroizing::new(*Hash32::digest("pq-identity-seed", identity_seed).as_bytes());
        let key = SigningKey::<MlDsa65>::new(&(*seed).into());
        let bytes = key.as_ref().encode().to_vec();
        let public = PqPublicKey::from_bytes(&bytes).expect("a generated key is canonical");
        Self { key, public }
    }

    pub fn public(&self) -> &PqPublicKey {
        &self.public
    }

    pub fn sign(&self, domain: &str, message: &[u8]) -> Vec<u8> {
        let signature: Signature<MlDsa65> = self.key.sign(&framed(domain, message));
        signature.encode().to_vec()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::NodeIdentity;

    #[test]
    fn derived_keys_are_stable_sized_and_distinct_per_identity() {
        let a = NodeIdentity::from_secret_bytes(&[1; 32]).pq_identity();
        let again = NodeIdentity::from_secret_bytes(&[1; 32]).pq_identity();
        let b = NodeIdentity::from_secret_bytes(&[2; 32]).pq_identity();
        assert_eq!(a.public(), again.public());
        assert_ne!(a.public(), b.public());
        assert_eq!(a.public().as_bytes().len(), PQ_PUBLIC_KEY_BYTES);
        assert_eq!(a.sign("d", b"m").len(), PQ_SIGNATURE_BYTES);
    }

    #[test]
    fn signatures_bind_key_domain_and_message() {
        let a = NodeIdentity::from_secret_bytes(&[1; 32]).pq_identity();
        let b = NodeIdentity::from_secret_bytes(&[2; 32]).pq_identity();
        let signature = a.sign("session-bind", b"hello");
        a.public()
            .verify("session-bind", b"hello", &signature)
            .unwrap();
        assert!(
            a.public()
                .verify("session-bind", b"hellp", &signature)
                .is_err()
        );
        assert!(a.public().verify("other", b"hello", &signature).is_err());
        assert!(
            b.public()
                .verify("session-bind", b"hello", &signature)
                .is_err()
        );
        let mut flipped = signature.clone();
        flipped[100] ^= 1;
        assert!(
            a.public()
                .verify("session-bind", b"hello", &flipped)
                .is_err()
        );
        assert!(
            a.public()
                .verify("session-bind", b"hello", &signature[..100])
                .is_err()
        );
    }

    #[test]
    fn public_keys_round_trip_and_reject_bad_encodings() {
        let a = NodeIdentity::from_secret_bytes(&[1; 32]).pq_identity();
        let bytes = a.public().as_bytes().to_vec();
        assert_eq!(&PqPublicKey::from_bytes(&bytes).unwrap(), a.public());
        assert!(PqPublicKey::from_bytes(&bytes[..1951]).is_err());
        assert!(PqPublicKey::from_bytes(&[]).is_err());
    }
}
