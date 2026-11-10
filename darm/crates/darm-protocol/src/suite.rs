//! Polymorphic negotiation: which cipher suite, codec and padding a session
//! uses.
//!
//! Each side lists what it supports in preference order. The selection for
//! every dimension is the mutually supported option with the lowest *sum of
//! ranks* across both lists, ties broken by the initiator's order. Summing
//! ranks lets a constrained device's preferences count as much as a server's
//! (a phone that ranks CBOR first still gets CBOR from a JSON-first server
//! that also supports it, if the phone ranks JSON last), and the rule is a
//! pure function of the two offers so both sides compute the same answer.
//!
//! Every suite here is authenticated encryption; there is no "null" suite to
//! downgrade to. Plaintext signed gossip is a separate, configured fallback
//! outside sessions.

use darm_core::canonical::Canonical;
use darm_core::{DarmError, Result};
use serde::{Deserialize, Serialize};

pub const MAX_OFFER_ITEMS: usize = 8;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Suite {
    /// Noise_XX_25519_ChaChaPoly_BLAKE2s: fast in software (phones, small
    /// ARM boards, CPUs without AES instructions).
    NoiseXxChaChaPoly,
    /// Noise_XX_25519_AESGCM_SHA256: fastest where AES is in hardware.
    NoiseXxAesGcm,
}

impl Suite {
    pub fn noise_params(self) -> &'static str {
        match self {
            Self::NoiseXxChaChaPoly => "Noise_XX_25519_ChaChaPoly_BLAKE2s",
            Self::NoiseXxAesGcm => "Noise_XX_25519_AESGCM_SHA256",
        }
    }

    fn code(self) -> u8 {
        match self {
            Self::NoiseXxChaChaPoly => 1,
            Self::NoiseXxAesGcm => 2,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Codec {
    /// Interoperable and debuggable.
    Json,
    /// Compact binary; preferred on metered and mobile links.
    Cbor,
}

impl Codec {
    fn code(self) -> u8 {
        match self {
            Self::Json => 1,
            Self::Cbor => 2,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Padding {
    /// Pad to the next power of two (minimum 128 bytes). Strongest length
    /// hiding, up to ~2x bandwidth.
    PowerOfTwo,
    /// Pad to a multiple of 256 bytes. Cheap, hides small differences.
    Block256,
    /// Pad to a multiple of 16 bytes only. For metered links.
    Minimal,
}

impl Padding {
    fn code(self) -> u8 {
        match self {
            Self::PowerOfTwo => 1,
            Self::Block256 => 2,
            Self::Minimal => 3,
        }
    }

    /// Padded length for `len` plaintext bytes, never above `cap`.
    pub fn padded_len(self, len: usize, cap: usize) -> usize {
        let padded = match self {
            Self::PowerOfTwo => len.max(128).next_power_of_two(),
            Self::Block256 => len.div_ceil(256).max(1) * 256,
            Self::Minimal => len.div_ceil(16).max(1) * 16,
        };
        padded.clamp(len, cap.max(len))
    }
}

/// What one side supports, most preferred first.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Offer {
    pub suites: Vec<Suite>,
    pub codecs: Vec<Codec>,
    pub paddings: Vec<Padding>,
}

impl Default for Offer {
    fn default() -> Self {
        Self {
            suites: vec![Suite::NoiseXxChaChaPoly, Suite::NoiseXxAesGcm],
            codecs: vec![Codec::Json, Codec::Cbor],
            paddings: vec![Padding::PowerOfTwo, Padding::Block256, Padding::Minimal],
        }
    }
}

fn check_list<T: PartialEq>(what: &'static str, items: &[T]) -> Result<()> {
    if items.is_empty() || items.len() > MAX_OFFER_ITEMS {
        return Err(DarmError::malformed(what, "must list 1..=8 options"));
    }
    for (i, item) in items.iter().enumerate() {
        if items[..i].contains(item) {
            return Err(DarmError::malformed(what, "duplicate option"));
        }
    }
    Ok(())
}

impl Offer {
    pub fn validate(&self) -> Result<()> {
        check_list("offer suites", &self.suites)?;
        check_list("offer codecs", &self.codecs)?;
        check_list("offer paddings", &self.paddings)
    }

    /// Canonical bytes, mixed into the Noise prologue so the handshake fails
    /// if anyone altered either offer in transit.
    pub fn encode(&self, encoder: &mut Canonical) {
        encoder.u32(self.suites.len() as u32);
        for suite in &self.suites {
            encoder.u8(suite.code());
        }
        encoder.u32(self.codecs.len() as u32);
        for codec in &self.codecs {
            encoder.u8(codec.code());
        }
        encoder.u32(self.paddings.len() as u32);
        for padding in &self.paddings {
            encoder.u8(padding.code());
        }
    }
}

/// The negotiated parameters of one session.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Selected {
    pub suite: Suite,
    pub codec: Codec,
    pub padding: Padding,
}

fn pick<T: Copy + PartialEq>(what: &'static str, initiator: &[T], responder: &[T]) -> Result<T> {
    initiator
        .iter()
        .enumerate()
        .filter_map(|(i, item)| {
            responder
                .iter()
                .position(|other| other == item)
                .map(|j| (i + j, i, *item))
        })
        .min_by_key(|(sum, i, _)| (*sum, *i))
        .map(|(_, _, item)| item)
        .ok_or_else(|| DarmError::Rejected(format!("no common {what}")))
}

/// Deterministic selection from two offers (see the module docs).
pub fn negotiate(initiator: &Offer, responder: &Offer) -> Result<Selected> {
    initiator.validate()?;
    responder.validate()?;
    Ok(Selected {
        suite: pick("suite", &initiator.suites, &responder.suites)?,
        codec: pick("codec", &initiator.codecs, &responder.codecs)?,
        padding: pick("padding", &initiator.paddings, &responder.paddings)?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rank_sum_respects_both_sides() {
        let server = Offer {
            suites: vec![Suite::NoiseXxAesGcm, Suite::NoiseXxChaChaPoly],
            codecs: vec![Codec::Json, Codec::Cbor],
            paddings: vec![Padding::PowerOfTwo, Padding::Block256, Padding::Minimal],
        };
        let phone = Offer {
            suites: vec![Suite::NoiseXxChaChaPoly],
            codecs: vec![Codec::Cbor, Codec::Json],
            paddings: vec![Padding::Minimal, Padding::Block256],
        };
        let selected = negotiate(&server, &phone).unwrap();
        assert_eq!(selected.suite, Suite::NoiseXxChaChaPoly);
        // Tie (0+1 vs 1+0): the initiator's order breaks it.
        assert_eq!(selected.codec, Codec::Json);
        assert_eq!(negotiate(&phone, &server).unwrap().codec, Codec::Cbor);
        // Block256 has rank sum 2 on both orders; Minimal is 2+0 / 0+2.
        let p = negotiate(&server, &phone).unwrap().padding;
        assert_eq!(p, Padding::Block256);
    }

    #[test]
    fn negotiation_is_deterministic_and_fails_closed() {
        let a = Offer::default();
        assert_eq!(negotiate(&a, &a).unwrap(), negotiate(&a, &a).unwrap());
        let aes_only = Offer {
            suites: vec![Suite::NoiseXxAesGcm],
            ..Offer::default()
        };
        let chacha_only = Offer {
            suites: vec![Suite::NoiseXxChaChaPoly],
            ..Offer::default()
        };
        assert!(negotiate(&aes_only, &chacha_only).is_err());
        let duplicate = Offer {
            codecs: vec![Codec::Json, Codec::Json],
            ..Offer::default()
        };
        assert!(negotiate(&duplicate, &a).is_err());
        let empty = Offer {
            paddings: vec![],
            ..Offer::default()
        };
        assert!(negotiate(&a, &empty).is_err());
    }

    #[test]
    fn padding_buckets() {
        assert_eq!(Padding::PowerOfTwo.padded_len(1, 65_519), 128);
        assert_eq!(Padding::PowerOfTwo.padded_len(129, 65_519), 256);
        assert_eq!(Padding::PowerOfTwo.padded_len(40_000, 65_519), 65_519);
        assert_eq!(Padding::Block256.padded_len(257, 65_519), 512);
        assert_eq!(Padding::Minimal.padded_len(17, 65_519), 32);
        // Never shorter than the input, even past the cap.
        assert_eq!(Padding::Minimal.padded_len(70_000, 65_519), 70_000);
    }
}
