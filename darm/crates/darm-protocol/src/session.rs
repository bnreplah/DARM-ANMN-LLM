//! Established sessions: sealing and opening morphing frames.
//!
//! Wire frame: `masked_nonce (8) || noise_ciphertext`.
//!
//! - The 64-bit nonce is explicit so frames survive gossip reordering and
//!   loss (Noise "stateless" transport), and is XOR-masked with a keyed
//!   BLAKE3 PRF over the first 16 ciphertext bytes, the way QUIC protects
//!   packet numbers. Frames therefore contain no counters or fixed bytes.
//! - The authenticated plaintext is
//!   `msg_id (u32) || frag_index (u16) || frag_count (u16) || len (u32) || data || zeros`,
//!   padded with the session's [`Padding`](crate::Padding) policy.
//! - A [`ReplayWindow`] is updated only after decryption succeeds.
//! - Messages above one Noise frame are fragmented; reassembly is bounded in
//!   fragments, bytes, concurrent partial messages and time.
//! - A session dies after a maximum lifetime or message count and must be
//!   re-handshaken, which also renews keys (forward secrecy per session).

use crate::MAX_MESSAGE_BYTES;
use crate::replay::ReplayWindow;
use crate::suite::{Codec, Selected};
use darm_core::{DarmError, NodeId, Result};
use serde::Serialize;
use serde::de::DeserializeOwned;
use std::collections::HashMap;
use zeroize::Zeroizing;

const NOISE_MAX: usize = 65_535;
const TAG: usize = 16;
const NONCE: usize = 8;
const HEADER: usize = 12;
/// Largest authenticated plaintext in one frame.
pub const MAX_FRAME_PLAINTEXT: usize = NOISE_MAX - TAG;
const MAX_FRAGMENT_DATA: usize = MAX_FRAME_PLAINTEXT - HEADER;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SessionLimits {
    pub max_lifetime_ms: u64,
    pub max_messages: u64,
    pub max_partial_messages: usize,
    pub reassembly_timeout_ms: u64,
}

impl Default for SessionLimits {
    fn default() -> Self {
        Self {
            max_lifetime_ms: 60 * 60 * 1_000,
            max_messages: 1 << 32,
            max_partial_messages: 8,
            reassembly_timeout_ms: 30_000,
        }
    }
}

struct Partial {
    fragments: Vec<Option<Vec<u8>>>,
    bytes: usize,
    started_ms: u64,
}

pub struct Session {
    sid: [u8; 16],
    peer: NodeId,
    selected: Selected,
    transport: snow::StatelessTransportState,
    send_nonce: u64,
    recv_window: ReplayWindow,
    hp_send: Zeroizing<[u8; 32]>,
    hp_recv: Zeroizing<[u8; 32]>,
    established_ms: u64,
    limits: SessionLimits,
    next_msg_id: u32,
    partial: HashMap<u32, Partial>,
    pub frames_sent: u64,
    pub frames_received: u64,
    pub frames_rejected: u64,
}

impl std::fmt::Debug for Session {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Session")
            .field("sid", &hex::encode(&self.sid[..4]))
            .field("peer", &self.peer)
            .field("selected", &self.selected)
            .finish_non_exhaustive()
    }
}

fn header_mask(key: &[u8; 32], ciphertext: &[u8]) -> [u8; NONCE] {
    let sample = &ciphertext[..TAG.min(ciphertext.len())];
    let digest = blake3::keyed_hash(key, sample);
    let mut mask = [0u8; NONCE];
    mask.copy_from_slice(&digest.as_bytes()[..NONCE]);
    mask
}

impl Session {
    pub(crate) fn establish(
        state: snow::HandshakeState,
        sid: [u8; 16],
        peer: NodeId,
        selected: Selected,
        initiator: bool,
        limits: SessionLimits,
        now_ms: u64,
    ) -> Result<Self> {
        let hash = state.get_handshake_hash().to_vec();
        let derive = |label: &str| {
            Zeroizing::new(blake3::derive_key(
                &format!("{} header-protection {label}", darm_core::PROTOCOL),
                &hash,
            ))
        };
        let (hp_send, hp_recv) = if initiator {
            (derive("i2r"), derive("r2i"))
        } else {
            (derive("r2i"), derive("i2r"))
        };
        let transport = state
            .into_stateless_transport_mode()
            .map_err(|e| DarmError::Rejected(format!("noise: {e}")))?;
        Ok(Self {
            sid,
            peer,
            selected,
            transport,
            send_nonce: 0,
            recv_window: ReplayWindow::default(),
            hp_send,
            hp_recv,
            established_ms: now_ms,
            limits,
            next_msg_id: 0,
            partial: HashMap::new(),
            frames_sent: 0,
            frames_received: 0,
            frames_rejected: 0,
        })
    }

    /// The DARM node proven to hold the other end of this session.
    pub fn peer(&self) -> NodeId {
        self.peer
    }

    pub fn selected(&self) -> Selected {
        self.selected
    }

    pub fn sid_hex(&self) -> String {
        hex::encode(self.sid)
    }

    pub fn established_ms(&self) -> u64 {
        self.established_ms
    }

    /// Whether the session must be replaced by a new handshake.
    pub fn expired(&self, now_ms: u64) -> bool {
        now_ms.saturating_sub(self.established_ms) >= self.limits.max_lifetime_ms
            || self.send_nonce >= self.limits.max_messages
    }

    pub fn encode<T: Serialize>(&self, value: &T) -> Result<Vec<u8>> {
        match self.selected.codec {
            Codec::Json => serde_json::to_vec(value)
                .map_err(|e| DarmError::malformed("message", e.to_string())),
            Codec::Cbor => {
                let mut out = Vec::new();
                ciborium::into_writer(value, &mut out)
                    .map_err(|e| DarmError::malformed("message", e.to_string()))?;
                Ok(out)
            }
        }
    }

    pub fn decode<T: DeserializeOwned>(&self, bytes: &[u8]) -> Result<T> {
        if bytes.len() > MAX_MESSAGE_BYTES {
            return Err(DarmError::LimitExceeded("message".into()));
        }
        match self.selected.codec {
            Codec::Json => serde_json::from_slice(bytes)
                .map_err(|e| DarmError::malformed("message", e.to_string())),
            Codec::Cbor => ciborium::from_reader(bytes)
                .map_err(|e| DarmError::malformed("message", e.to_string())),
        }
    }

    /// Seal `message` into one or more frames.
    pub fn seal(&mut self, message: &[u8], now_ms: u64) -> Result<Vec<Vec<u8>>> {
        if message.len() > MAX_MESSAGE_BYTES {
            return Err(DarmError::LimitExceeded("message".into()));
        }
        if self.expired(now_ms) {
            return Err(DarmError::Rejected("session expired".into()));
        }
        let chunks: Vec<&[u8]> = if message.is_empty() {
            vec![&[][..]]
        } else {
            message.chunks(MAX_FRAGMENT_DATA).collect()
        };
        let count = u16::try_from(chunks.len())
            .map_err(|_| DarmError::LimitExceeded("fragments".into()))?;
        let msg_id = self.next_msg_id;
        self.next_msg_id = self.next_msg_id.wrapping_add(1);
        let mut frames = Vec::with_capacity(chunks.len());
        for (index, chunk) in chunks.into_iter().enumerate() {
            let mut plain = Vec::with_capacity(HEADER + chunk.len());
            plain.extend_from_slice(&msg_id.to_be_bytes());
            plain.extend_from_slice(&(index as u16).to_be_bytes());
            plain.extend_from_slice(&count.to_be_bytes());
            plain.extend_from_slice(&(chunk.len() as u32).to_be_bytes());
            plain.extend_from_slice(chunk);
            let padded = self
                .selected
                .padding
                .padded_len(plain.len(), MAX_FRAME_PLAINTEXT);
            plain.resize(padded, 0);
            let nonce = self.send_nonce;
            self.send_nonce += 1;
            let mut frame = vec![0u8; NONCE + plain.len() + TAG];
            let written = self
                .transport
                .write_message(nonce, &plain, &mut frame[NONCE..])
                .map_err(|e| DarmError::Rejected(format!("noise: {e}")))?;
            frame.truncate(NONCE + written);
            let mask = header_mask(&self.hp_send, &frame[NONCE..]);
            for (i, byte) in nonce.to_be_bytes().iter().enumerate() {
                frame[i] = byte ^ mask[i];
            }
            frames.push(frame);
        }
        self.frames_sent += frames.len() as u64;
        Ok(frames)
    }

    /// Open one frame. Returns a complete message once all of its fragments
    /// have arrived.
    pub fn open(&mut self, frame: &[u8], now_ms: u64) -> Result<Option<Vec<u8>>> {
        match self.open_inner(frame, now_ms) {
            Ok(message) => {
                self.frames_received += 1;
                Ok(message)
            }
            Err(error) => {
                self.frames_rejected += 1;
                Err(error)
            }
        }
    }

    fn open_inner(&mut self, frame: &[u8], now_ms: u64) -> Result<Option<Vec<u8>>> {
        if frame.len() < NONCE + TAG || frame.len() > NONCE + NOISE_MAX {
            return Err(DarmError::malformed("frame", "bad length"));
        }
        let ciphertext = &frame[NONCE..];
        let mask = header_mask(&self.hp_recv, ciphertext);
        let mut nonce_bytes = [0u8; NONCE];
        for (i, byte) in nonce_bytes.iter_mut().enumerate() {
            *byte = frame[i] ^ mask[i];
        }
        let nonce = u64::from_be_bytes(nonce_bytes);
        if !self.recv_window.check(nonce) {
            return Err(DarmError::Rejected("replayed frame".into()));
        }
        let mut plain = vec![0u8; ciphertext.len()];
        let len = self
            .transport
            .read_message(nonce, ciphertext, &mut plain)
            .map_err(|_| DarmError::InvalidSignature)?;
        // Only an authentic frame may move the window.
        self.recv_window.commit(nonce);
        plain.truncate(len);
        if plain.len() < HEADER {
            return Err(DarmError::malformed("frame", "short plaintext"));
        }
        let msg_id = u32::from_be_bytes(plain[0..4].try_into().expect("4 bytes"));
        let index = usize::from(u16::from_be_bytes(plain[4..6].try_into().expect("2 bytes")));
        let count = usize::from(u16::from_be_bytes(plain[6..8].try_into().expect("2 bytes")));
        let data_len = u32::from_be_bytes(plain[8..12].try_into().expect("4 bytes")) as usize;
        if count == 0 || index >= count || HEADER + data_len > plain.len() {
            return Err(DarmError::malformed("frame", "bad fragment header"));
        }
        if count.saturating_mul(MAX_FRAGMENT_DATA) > MAX_MESSAGE_BYTES + MAX_FRAGMENT_DATA {
            return Err(DarmError::LimitExceeded("fragment count".into()));
        }
        let data = plain[HEADER..HEADER + data_len].to_vec();
        if count == 1 {
            return Ok(Some(data));
        }
        self.reassemble(msg_id, index, count, data, now_ms)
    }

    fn reassemble(
        &mut self,
        msg_id: u32,
        index: usize,
        count: usize,
        data: Vec<u8>,
        now_ms: u64,
    ) -> Result<Option<Vec<u8>>> {
        let timeout = self.limits.reassembly_timeout_ms;
        self.partial
            .retain(|_, p| now_ms.saturating_sub(p.started_ms) < timeout);
        if !self.partial.contains_key(&msg_id)
            && self.partial.len() >= self.limits.max_partial_messages
        {
            return Err(DarmError::LimitExceeded("partial messages".into()));
        }
        let partial = self.partial.entry(msg_id).or_insert_with(|| Partial {
            fragments: vec![None; count],
            bytes: 0,
            started_ms: now_ms,
        });
        if partial.fragments.len() != count {
            self.partial.remove(&msg_id);
            return Err(DarmError::malformed("frame", "fragment count changed"));
        }
        if partial.fragments[index].is_some() {
            return Err(DarmError::Duplicate);
        }
        partial.bytes += data.len();
        if partial.bytes > MAX_MESSAGE_BYTES {
            self.partial.remove(&msg_id);
            return Err(DarmError::LimitExceeded("reassembled message".into()));
        }
        partial.fragments[index] = Some(data);
        if partial.fragments.iter().any(Option::is_none) {
            return Ok(None);
        }
        let partial = self.partial.remove(&msg_id).expect("present");
        Ok(Some(
            partial.fragments.into_iter().flatten().flatten().collect(),
        ))
    }
}
