//! The envelope that multiplexes channels inside a sealed session message.
//!
//! ```text
//! u8 version (1) | u16 channel | u8 flags | u32 stream | u32 seq | payload
//! ```
//!
//! All integers are big-endian. `stream` groups related messages on a channel
//! (one tensor, one job); `seq` counts messages the sender has put on that
//! channel. The envelope is not signed on its own: it only ever travels
//! inside a session frame, which authenticates it and its sender.

use crate::channel::ChannelId;
use darm_core::{DarmError, Result};

pub const VERSION: u8 = 1;
pub const HEADER_LEN: usize = 12;

/// The last message of its stream.
pub const FLAG_FIN: u8 = 0b0000_0001;
const KNOWN_FLAGS: u8 = FLAG_FIN;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Envelope {
    pub channel: ChannelId,
    pub flags: u8,
    pub stream: u32,
    pub seq: u32,
    pub payload: Vec<u8>,
}

impl Envelope {
    pub fn fin(&self) -> bool {
        self.flags & FLAG_FIN != 0
    }

    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(HEADER_LEN + self.payload.len());
        out.push(VERSION);
        out.extend_from_slice(&self.channel.0.to_be_bytes());
        out.push(self.flags);
        out.extend_from_slice(&self.stream.to_be_bytes());
        out.extend_from_slice(&self.seq.to_be_bytes());
        out.extend_from_slice(&self.payload);
        out
    }

    /// Decode an envelope. Unknown versions and reserved flag bits are
    /// rejected so they stay available for future use.
    pub fn decode(bytes: &[u8]) -> Result<Self> {
        let Some((header, payload)) = bytes.split_first_chunk::<HEADER_LEN>() else {
            return Err(DarmError::malformed("envelope", "shorter than header"));
        };
        if header[0] != VERSION {
            return Err(DarmError::malformed("envelope", "unsupported version"));
        }
        let flags = header[3];
        if flags & !KNOWN_FLAGS != 0 {
            return Err(DarmError::malformed("envelope", "reserved flag set"));
        }
        Ok(Self {
            channel: ChannelId(u16::from_be_bytes([header[1], header[2]])),
            flags,
            stream: u32::from_be_bytes([header[4], header[5], header[6], header[7]]),
            seq: u32::from_be_bytes([header[8], header[9], header[10], header[11]]),
            payload: payload.to_vec(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn layout_is_stable() {
        let envelope = Envelope {
            channel: ChannelId(0x0102),
            flags: FLAG_FIN,
            stream: 0x0304_0506,
            seq: 0x0708_090A,
            payload: vec![0xAA, 0xBB],
        };
        let bytes = envelope.encode();
        assert_eq!(
            bytes,
            [1, 1, 2, 1, 3, 4, 5, 6, 7, 8, 9, 10, 0xAA, 0xBB],
            "wire layout must not change within version 1"
        );
        assert_eq!(Envelope::decode(&bytes).unwrap(), envelope);
    }

    #[test]
    fn rejects_short_unknown_version_and_reserved_flags() {
        assert!(Envelope::decode(&[1; HEADER_LEN - 1]).is_err());
        let mut bytes = Envelope {
            channel: ChannelId(1),
            flags: 0,
            stream: 0,
            seq: 0,
            payload: Vec::new(),
        }
        .encode();
        assert!(Envelope::decode(&bytes).is_ok());
        bytes[0] = 2;
        assert!(Envelope::decode(&bytes).is_err());
        bytes[0] = VERSION;
        bytes[3] = 0b1000_0000;
        assert!(Envelope::decode(&bytes).is_err());
    }
}
