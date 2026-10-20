//! Gossip messages on the `darm.v1` mesh channel.
//!
//! Mesh plugin frames do not authenticate their claimed source peer, so no
//! message here is trusted for who sent it: every memory, entry, block and
//! attestation carries its own Ed25519 signature and is verified on receipt.
//! Messages are bounded before and after parsing.

use darm_compute::SignedAdvert;
use darm_core::Hash32;
use darm_core::ledger::{Attestation, Block, SignedEntry};
use darm_memory::{SignedMemory, SignedTombstone};
use darm_protocol::HandshakeMsg;
use serde::{Deserialize, Serialize};

pub const CHANNEL: &str = "darm.v1";
pub const CONTENT_TYPE: &str = "application/json";
/// Largest channel body accepted (checked before JSON parsing).
pub const MAX_MESSAGE_BYTES: usize = 1024 * 1024;
pub const MAX_IDS: usize = 512;
pub const MAX_MEMORIES_PER_PUSH: usize = 64;
pub const MAX_BLOCKS_PER_SYNC: usize = 32;
/// Base64 of one Noise frame (8-byte masked nonce + up to 64 KiB).
pub const MAX_SEALED_FRAME_CHARS: usize = (8usize + 65_535).div_ceil(3) * 4;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Wire {
    /// Shareable memory ids this node holds (anti-entropy).
    MemoryAnnounce {
        ids: Vec<Hash32>,
    },
    MemoryRequest {
        ids: Vec<Hash32>,
    },
    MemoryPush {
        memories: Vec<SignedMemory>,
    },
    Tombstone {
        tombstone: SignedTombstone,
    },
    LedgerEntry {
        entry: SignedEntry,
    },
    BlockProposal {
        block: Block,
    },
    Attestation {
        block_hash: Hash32,
        attestation: Attestation,
    },
    SyncRequest {
        chain: Hash32,
        from_height: u64,
    },
    SyncBlocks {
        blocks: Vec<Block>,
    },
    /// What this node offers to the compute pool (CPU/GPU/NPU).
    ComputeAdvert {
        advert: SignedAdvert,
    },
    /// Noise session handshake (the only plaintext allowed when sessions
    /// are required).
    Handshake {
        handshake: HandshakeMsg,
    },
    /// One sealed session frame carrying another `Wire` message.
    Sealed {
        frame: String,
    },
}

impl Wire {
    pub fn kind(&self) -> &'static str {
        match self {
            Self::MemoryAnnounce { .. } => "memory_announce",
            Self::MemoryRequest { .. } => "memory_request",
            Self::MemoryPush { .. } => "memory_push",
            Self::Tombstone { .. } => "tombstone",
            Self::LedgerEntry { .. } => "ledger_entry",
            Self::BlockProposal { .. } => "block_proposal",
            Self::Attestation { .. } => "attestation",
            Self::SyncRequest { .. } => "sync_request",
            Self::SyncBlocks { .. } => "sync_blocks",
            Self::ComputeAdvert { .. } => "compute_advert",
            Self::Handshake { .. } => "handshake",
            Self::Sealed { .. } => "sealed",
        }
    }

    pub fn decode(body: &[u8]) -> Result<Self, String> {
        if body.len() > MAX_MESSAGE_BYTES {
            return Err(format!("message of {} bytes exceeds limit", body.len()));
        }
        let message: Self = serde_json::from_slice(body).map_err(|e| e.to_string())?;
        let within = match &message {
            Self::MemoryAnnounce { ids } | Self::MemoryRequest { ids } => ids.len() <= MAX_IDS,
            Self::MemoryPush { memories } => memories.len() <= MAX_MEMORIES_PER_PUSH,
            Self::SyncBlocks { blocks } => blocks.len() <= MAX_BLOCKS_PER_SYNC,
            Self::Sealed { frame } => frame.len() <= MAX_SEALED_FRAME_CHARS,
            _ => true,
        };
        if !within {
            return Err(format!("{} exceeds item limit", message.kind()));
        }
        Ok(message)
    }

    /// Messages that may never travel inside a sealed frame.
    pub fn is_session_control(&self) -> bool {
        matches!(self, Self::Handshake { .. } | Self::Sealed { .. })
    }
}

/// A message to send: `target` empty means broadcast to direct peers.
#[derive(Debug, Clone)]
pub struct Outbound {
    pub target: String,
    pub message: Wire,
}

impl Outbound {
    pub fn broadcast(message: Wire) -> Self {
        Self {
            target: String::new(),
            message,
        }
    }

    pub fn to(target: impl Into<String>, message: Wire) -> Self {
        Self {
            target: target.into(),
            message,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_oversized_and_overfull_messages() {
        assert!(Wire::decode(&vec![b' '; MAX_MESSAGE_BYTES + 1]).is_err());
        let ids = vec![Hash32::ZERO; MAX_IDS + 1];
        let body = serde_json::to_vec(&Wire::MemoryAnnounce { ids }).unwrap();
        assert!(Wire::decode(&body).is_err());
        let ok = serde_json::to_vec(&Wire::MemoryRequest {
            ids: vec![Hash32::ZERO],
        })
        .unwrap();
        assert!(Wire::decode(&ok).is_ok());
        assert!(Wire::decode(br#"{"type":"unknown"}"#).is_err());
    }
}
