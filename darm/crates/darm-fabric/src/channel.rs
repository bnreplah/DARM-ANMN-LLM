//! Channels and their sync classes.
//!
//! A channel is a named lane on a link. Its class is never sent on the wire:
//! both ends look it up in their own table, so a peer cannot promote its bulk
//! traffic to `tight` by setting a field.

use darm_core::{DarmError, Result};
use std::collections::BTreeMap;

/// Largest envelope payload that still fits one sealed session frame. `tight`
/// and `pipeline` channels are held to this so a message is never fragmented
/// and never waits behind its own tail.
pub const MAX_SINGLE_FRAME_PAYLOAD: usize =
    darm_protocol::session::MAX_FRAME_PLAINTEXT - 12 - crate::envelope::HEADER_LEN;

/// Largest payload any channel may carry (the session reassembly limit less
/// the envelope header).
pub const MAX_PAYLOAD: usize = darm_protocol::MAX_MESSAGE_BYTES - crate::envelope::HEADER_LEN;

/// Sync class of a channel, using the DARM-ANN protocol codes (spec section
/// 8, "sync class").
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum SyncClass {
    /// Background traffic with no deadline: anti-entropy, archives.
    Async = 0,
    /// Must land within a block interval: ledger entries, votes.
    Block = 1,
    /// Step-to-step traffic between pipeline stages: job shards, receipts.
    Pipeline = 2,
    /// Inner-loop traffic that is useless when late: activations, KV state.
    Tight = 3,
}

impl SyncClass {
    pub const ALL: [Self; 4] = [Self::Async, Self::Block, Self::Pipeline, Self::Tight];

    pub fn index(self) -> usize {
        self as usize
    }

    /// Whether messages of this class must fit a single sealed frame.
    pub fn single_frame(self) -> bool {
        matches!(self, Self::Pipeline | Self::Tight)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ChannelId(pub u16);

impl ChannelId {
    /// `darm.v1` gossip messages (memory anti-entropy, adverts).
    pub const GOSSIP: Self = Self(1);
    /// Ledger entries, block proposals and attestations.
    pub const LEDGER: Self = Self(2);
    /// Compute job shards and receipts.
    pub const COMPUTE: Self = Self(3);
    /// Tensor and activation streams ([`crate::tensor`]).
    pub const TENSOR: Self = Self(16);
    /// First id available to applications; lower ids are reserved.
    pub const APPLICATION_BASE: u16 = 0x8000;

    pub fn is_reserved(self) -> bool {
        self.0 < Self::APPLICATION_BASE
    }
}

/// Local policy for one channel.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChannelSpec {
    pub id: ChannelId,
    pub name: &'static str,
    pub class: SyncClass,
    /// Largest payload accepted or sent, in bytes.
    pub max_payload: usize,
    /// Inbound token bucket: burst size and refill per second.
    pub burst: u32,
    pub per_second: u32,
    /// Outbound messages older than this are dropped instead of sent.
    /// `None` never expires.
    pub ttl_ms: Option<u64>,
}

impl ChannelSpec {
    pub fn validate(&self) -> Result<()> {
        let cap = if self.class.single_frame() {
            MAX_SINGLE_FRAME_PAYLOAD
        } else {
            MAX_PAYLOAD
        };
        if self.max_payload == 0 || self.max_payload > cap {
            return Err(DarmError::LimitExceeded(format!(
                "channel {} max_payload must be 1..={cap}",
                self.name
            )));
        }
        if self.burst == 0 || self.per_second == 0 {
            return Err(DarmError::malformed("channel", "zero rate limit"));
        }
        if self.ttl_ms == Some(0) {
            return Err(DarmError::malformed("channel", "zero ttl"));
        }
        Ok(())
    }
}

/// The channels a link will speak. Anything else is refused.
#[derive(Debug, Clone, Default)]
pub struct ChannelTable {
    specs: BTreeMap<ChannelId, ChannelSpec>,
}

impl ChannelTable {
    /// The built-in DARM channels with reference limits.
    pub fn standard() -> Self {
        let mut table = Self::default();
        let builtin = [
            ChannelSpec {
                id: ChannelId::GOSSIP,
                name: "gossip",
                class: SyncClass::Async,
                max_payload: MAX_PAYLOAD,
                burst: 200,
                per_second: 50,
                ttl_ms: None,
            },
            ChannelSpec {
                id: ChannelId::LEDGER,
                name: "ledger",
                class: SyncClass::Block,
                max_payload: MAX_PAYLOAD,
                burst: 200,
                per_second: 50,
                ttl_ms: None,
            },
            ChannelSpec {
                id: ChannelId::COMPUTE,
                name: "compute",
                class: SyncClass::Pipeline,
                max_payload: MAX_SINGLE_FRAME_PAYLOAD,
                burst: 2_000,
                per_second: 1_000,
                ttl_ms: Some(30_000),
            },
            ChannelSpec {
                id: ChannelId::TENSOR,
                name: "tensor",
                class: SyncClass::Tight,
                max_payload: MAX_SINGLE_FRAME_PAYLOAD,
                burst: 20_000,
                per_second: 20_000,
                ttl_ms: Some(250),
            },
        ];
        for spec in builtin {
            table.specs.insert(spec.id, spec);
        }
        table
    }

    /// Mount an application channel. Reserved ids and duplicates are refused
    /// so an application cannot shadow a built-in channel.
    pub fn register(&mut self, spec: ChannelSpec) -> Result<()> {
        spec.validate()?;
        if spec.id.is_reserved() {
            return Err(DarmError::Unauthorized(format!(
                "channel id {} is reserved",
                spec.id.0
            )));
        }
        if self.specs.contains_key(&spec.id) {
            return Err(DarmError::Duplicate);
        }
        self.specs.insert(spec.id, spec);
        Ok(())
    }

    pub fn get(&self, id: ChannelId) -> Option<&ChannelSpec> {
        self.specs.get(&id)
    }

    pub fn iter(&self) -> impl Iterator<Item = &ChannelSpec> {
        self.specs.values()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn app(id: u16, class: SyncClass, max_payload: usize) -> ChannelSpec {
        ChannelSpec {
            id: ChannelId(id),
            name: "app",
            class,
            max_payload,
            burst: 10,
            per_second: 10,
            ttl_ms: None,
        }
    }

    #[test]
    fn standard_channels_are_valid() {
        for spec in ChannelTable::standard().iter() {
            spec.validate().unwrap();
        }
    }

    #[test]
    fn reserved_and_duplicate_ids_are_refused() {
        let mut table = ChannelTable::standard();
        assert!(matches!(
            table.register(app(ChannelId::TENSOR.0, SyncClass::Tight, 16)),
            Err(DarmError::Unauthorized(_))
        ));
        table.register(app(0x8000, SyncClass::Async, 16)).unwrap();
        assert_eq!(
            table.register(app(0x8000, SyncClass::Async, 16)),
            Err(DarmError::Duplicate)
        );
    }

    #[test]
    fn tight_channels_cannot_exceed_one_frame() {
        let mut table = ChannelTable::default();
        assert!(
            table
                .register(app(0x8001, SyncClass::Tight, MAX_SINGLE_FRAME_PAYLOAD + 1))
                .is_err()
        );
        table
            .register(app(0x8001, SyncClass::Tight, MAX_SINGLE_FRAME_PAYLOAD))
            .unwrap();
        table
            .register(app(0x8002, SyncClass::Async, MAX_PAYLOAD))
            .unwrap();
        assert!(
            table
                .register(app(0x8003, SyncClass::Async, MAX_PAYLOAD + 1))
                .is_err()
        );
    }
}
