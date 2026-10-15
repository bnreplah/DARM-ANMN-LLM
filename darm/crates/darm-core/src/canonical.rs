//! Canonical byte encoding for everything DARM-ANN signs or hashes.
//!
//! The format is deliberately boring so that a Python, Go or Solidity
//! implementation can reproduce it exactly:
//!
//! - the stream starts with the domain: `len(u32 BE) || "DARM-ANN/v1/" || domain`
//! - integers are fixed-width big-endian
//! - byte strings and UTF-8 strings are `len(u32 BE) || bytes`
//! - fixed 32-byte values (hashes, public keys) are written raw
//! - optional values are a `0`/`1` tag byte followed by the value
//! - lists are `count(u32 BE)` followed by each item
//!
//! Floating point never appears in signed data; callers scale to integers.
//! Domain separation means a signature over a memory record can never be
//! replayed as a signature over a block, vote or attestation.

use crate::PROTOCOL;

#[derive(Debug, Clone)]
pub struct Canonical {
    buf: Vec<u8>,
}

impl Canonical {
    pub fn new(domain: &str) -> Self {
        let mut encoder = Self {
            buf: Vec::with_capacity(128),
        };
        encoder.str(&format!("{PROTOCOL}/{domain}"));
        encoder
    }

    pub fn u8(&mut self, value: u8) -> &mut Self {
        self.buf.push(value);
        self
    }

    pub fn bool(&mut self, value: bool) -> &mut Self {
        self.u8(u8::from(value))
    }

    pub fn u16(&mut self, value: u16) -> &mut Self {
        self.buf.extend_from_slice(&value.to_be_bytes());
        self
    }

    pub fn i16(&mut self, value: i16) -> &mut Self {
        self.buf.extend_from_slice(&value.to_be_bytes());
        self
    }

    pub fn u32(&mut self, value: u32) -> &mut Self {
        self.buf.extend_from_slice(&value.to_be_bytes());
        self
    }

    pub fn i32(&mut self, value: i32) -> &mut Self {
        self.buf.extend_from_slice(&value.to_be_bytes());
        self
    }

    pub fn u64(&mut self, value: u64) -> &mut Self {
        self.buf.extend_from_slice(&value.to_be_bytes());
        self
    }

    /// Length-prefixed bytes. Inputs over `u32::MAX` bytes are rejected by
    /// every caller's size limits long before they reach the encoder, so the
    /// length saturates rather than panicking.
    pub fn bytes(&mut self, value: &[u8]) -> &mut Self {
        let len = u32::try_from(value.len()).unwrap_or(u32::MAX);
        self.u32(len);
        self.buf.extend_from_slice(value);
        self
    }

    pub fn str(&mut self, value: &str) -> &mut Self {
        self.bytes(value.as_bytes())
    }

    pub fn fixed32(&mut self, value: &[u8; 32]) -> &mut Self {
        self.buf.extend_from_slice(value);
        self
    }

    pub fn opt_fixed32(&mut self, value: Option<&[u8; 32]>) -> &mut Self {
        match value {
            Some(value) => self.u8(1).fixed32(value),
            None => self.u8(0),
        }
    }

    pub fn str_list<S: AsRef<str>>(&mut self, values: &[S]) -> &mut Self {
        self.u32(u32::try_from(values.len()).unwrap_or(u32::MAX));
        for value in values {
            self.str(value.as_ref());
        }
        self
    }

    pub fn as_bytes(&self) -> &[u8] {
        &self.buf
    }

    pub fn finish(self) -> Vec<u8> {
        self.buf
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn layout_is_stable() {
        let mut encoder = Canonical::new("t");
        encoder.u16(0x0102).str("hi").bool(true).opt_fixed32(None);
        let domain = b"DARM-ANN/v1/t";
        let mut expected = Vec::new();
        expected.extend_from_slice(&(domain.len() as u32).to_be_bytes());
        expected.extend_from_slice(domain);
        expected.extend_from_slice(&[0x01, 0x02]);
        expected.extend_from_slice(&2u32.to_be_bytes());
        expected.extend_from_slice(b"hi");
        expected.extend_from_slice(&[1, 0]);
        assert_eq!(encoder.finish(), expected);
    }

    #[test]
    fn domains_never_collide() {
        assert_ne!(
            Canonical::new("a").finish(),
            Canonical::new("b").finish(),
            "distinct domains must encode differently"
        );
    }
}
