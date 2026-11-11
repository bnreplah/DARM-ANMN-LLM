//! ML-KEM-768 for hybrid post-quantum sessions.
//!
//! Classical sessions rest on X25519 alone: someone who records the traffic
//! today and later has a large quantum computer could recover the session
//! keys. A hybrid suite adds an ML-KEM-768 (FIPS 203) exchange and mixes its
//! shared secret into the same Noise handshake as a pre-shared key, so the
//! session keys depend on both. Breaking the session then needs X25519 *and*
//! ML-KEM to fail.
//!
//! ```text
//!  initiator                                          responder
//!  -> e, payload: ML-KEM encapsulation key   ───────►
//!                                            ◄───────  <- e, ee, s, es,
//!                                                      payload: {.., kem ciphertext}
//!  -> s, se, psk                             ───────►   psk = H(ML-KEM shared secret)
//! ```
//!
//! The encapsulation key and the ciphertext travel in Noise payloads, so
//! both are covered by the handshake hash that each side signs with its
//! node identity: altering either one fails the handshake.
//!
//! What this does not change: authentication is still classical (Ed25519
//! node identities, X25519 static keys). An attacker with a quantum computer
//! *during* the handshake could impersonate a node; recorded traffic stays
//! confidential. That is the same position as hybrid TLS 1.3 today.
//!
//! The ML-KEM implementation is the RustCrypto `ml-kem` crate, which has not
//! had an independent security audit.

use darm_core::{DarmError, Hash32, Result};
use ml_kem::kem::{Decapsulate, Encapsulate, Kem, KeyExport};
use ml_kem::{Ciphertext, MlKem768};
use zeroize::Zeroizing;

pub const ENCAPSULATION_KEY_LEN: usize = 1184;
pub const CIPHERTEXT_LEN: usize = 1088;

type Dk = <MlKem768 as Kem>::DecapsulationKey;
type Ek = <MlKem768 as Kem>::EncapsulationKey;

/// The 32-byte pre-shared key mixed into Noise at `psk3`.
pub type Psk = Zeroizing<[u8; 32]>;

fn psk_from(shared: &[u8]) -> Psk {
    // Domain-separate the KEM output from its use as a Noise PSK.
    Zeroizing::new(*Hash32::digest("session-hybrid-psk", shared).as_bytes())
}

/// The initiator's half of the exchange, kept until the ciphertext arrives.
pub struct KemSecret(Dk);

impl std::fmt::Debug for KemSecret {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("KemSecret(..)")
    }
}

/// Generate an ephemeral key pair; returns the secret half and the encoded
/// encapsulation key to send.
pub fn generate() -> (KemSecret, Vec<u8>) {
    let (dk, ek) = MlKem768::generate_keypair();
    (KemSecret(dk), ek.to_bytes().to_vec())
}

/// Responder: encapsulate to the initiator's key. Returns the ciphertext to
/// send and the pre-shared key. The key is validated as FIPS 203 requires.
pub fn encapsulate(encapsulation_key: &[u8]) -> Result<(Vec<u8>, Psk)> {
    let bytes = encapsulation_key
        .try_into()
        .map_err(|_| DarmError::malformed("ml-kem key", "expected 1184 bytes"))?;
    let ek = Ek::new(bytes).map_err(|_| DarmError::malformed("ml-kem key", "invalid key"))?;
    let (ciphertext, shared) = ek.encapsulate();
    Ok((ciphertext.to_vec(), psk_from(&shared)))
}

impl KemSecret {
    /// Initiator: recover the pre-shared key from the responder's
    /// ciphertext. ML-KEM never reports a bad ciphertext; it yields an
    /// unrelated secret, and the Noise handshake then fails to authenticate.
    pub fn decapsulate(&self, ciphertext: &[u8]) -> Result<Psk> {
        let ciphertext: Ciphertext<MlKem768> = ciphertext
            .try_into()
            .map_err(|_| DarmError::malformed("ml-kem ciphertext", "expected 1088 bytes"))?;
        Ok(psk_from(&self.0.decapsulate(&ciphertext)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn both_sides_derive_the_same_key() {
        let (secret, key) = generate();
        assert_eq!(key.len(), ENCAPSULATION_KEY_LEN);
        let (ciphertext, responder) = encapsulate(&key).unwrap();
        assert_eq!(ciphertext.len(), CIPHERTEXT_LEN);
        assert_eq!(*secret.decapsulate(&ciphertext).unwrap(), *responder);
    }

    #[test]
    fn each_exchange_is_fresh() {
        let (secret, key) = generate();
        let (ct_a, psk_a) = encapsulate(&key).unwrap();
        let (ct_b, psk_b) = encapsulate(&key).unwrap();
        assert_ne!(ct_a, ct_b);
        assert_ne!(*psk_a, *psk_b);
        let (other, _) = generate();
        assert_ne!(*other.decapsulate(&ct_a).unwrap(), *psk_a);
        assert_eq!(*secret.decapsulate(&ct_b).unwrap(), *psk_b);
    }

    #[test]
    fn a_tampered_ciphertext_yields_a_different_key_not_an_error() {
        let (secret, key) = generate();
        let (mut ciphertext, psk) = encapsulate(&key).unwrap();
        ciphertext[10] ^= 1;
        assert_ne!(*secret.decapsulate(&ciphertext).unwrap(), *psk);
    }

    #[test]
    fn malformed_keys_and_ciphertexts_are_rejected() {
        let (secret, key) = generate();
        assert!(encapsulate(&key[..key.len() - 1]).is_err());
        assert!(encapsulate(&[]).is_err());
        // Coefficients must be reduced mod q = 3329; all-ones 12-bit values
        // (4095) are not, so FIPS 203's key check must refuse this.
        let mut unreduced = key.clone();
        unreduced[..64].fill(0xFF);
        assert!(encapsulate(&unreduced).is_err());
        assert!(secret.decapsulate(&[0; CIPHERTEXT_LEN - 1]).is_err());
        assert!(secret.decapsulate(&[]).is_err());
    }
}
