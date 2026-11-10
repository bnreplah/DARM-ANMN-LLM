//! One fabric link: a session plus channel policy.
//!
//! A [`Link`] is a state machine with no I/O. Outbound: [`Link::send`]
//! queues a message on a channel and [`Link::poll_outbound`] yields sealed
//! frames in priority order. Inbound: [`Link::on_frame`] opens a frame and
//! returns the message, already attributed to the proven peer and checked
//! against the channel's policy.

use crate::channel::{ChannelId, ChannelSpec, ChannelTable};
use crate::envelope::{Envelope, FLAG_FIN};
use crate::scheduler::{QueueLimits, Scheduler};
use darm_core::{DarmError, NodeId, Result};
use darm_protocol::Session;
use std::collections::HashMap;

/// A message received on a link.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Inbound {
    /// The node proven by the session handshake, not a claimed sender.
    pub peer: NodeId,
    pub channel: ChannelId,
    pub stream: u32,
    pub seq: u32,
    pub fin: bool,
    pub payload: Vec<u8>,
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct LinkStats {
    pub delivered: u64,
    pub rejected_frames: u64,
    pub unknown_channel: u64,
    pub oversized: u64,
    pub rate_limited: u64,
}

#[derive(Debug)]
struct Bucket {
    tokens_milli: u64,
    updated_ms: u64,
}

impl Bucket {
    fn take(&mut self, spec: &ChannelSpec, now_ms: u64) -> bool {
        let cap = u64::from(spec.burst) * 1_000;
        let elapsed = now_ms.saturating_sub(self.updated_ms);
        // tokens_milli gains per_second * 1000 per 1000 ms.
        let refill = elapsed.saturating_mul(u64::from(spec.per_second));
        self.tokens_milli = self.tokens_milli.saturating_add(refill).min(cap);
        self.updated_ms = self.updated_ms.max(now_ms);
        if self.tokens_milli < 1_000 {
            return false;
        }
        self.tokens_milli -= 1_000;
        true
    }
}

#[derive(Debug)]
pub struct Link {
    session: Session,
    channels: ChannelTable,
    scheduler: Scheduler,
    next_seq: HashMap<ChannelId, u32>,
    buckets: HashMap<ChannelId, Bucket>,
    pub stats: LinkStats,
}

impl Link {
    pub fn new(session: Session, channels: ChannelTable, limits: QueueLimits) -> Self {
        Self {
            session,
            channels,
            scheduler: Scheduler::new(limits),
            next_seq: HashMap::new(),
            buckets: HashMap::new(),
            stats: LinkStats::default(),
        }
    }

    /// The node at the other end, as proven by the handshake.
    pub fn peer(&self) -> NodeId {
        self.session.peer()
    }

    pub fn session(&self) -> &Session {
        &self.session
    }

    pub fn scheduler(&self) -> &Scheduler {
        &self.scheduler
    }

    /// Bytes that can still be queued on `channel`'s class.
    pub fn free_outbound_bytes(&self, channel: ChannelId) -> Result<usize> {
        let spec = self
            .channels
            .get(channel)
            .ok_or_else(|| DarmError::Rejected(format!("channel {} is not mounted", channel.0)))?;
        Ok(self.scheduler.free_bytes(spec.class))
    }

    /// Whether the session must be replaced before more traffic is sent.
    pub fn expired(&self, now_ms: u64) -> bool {
        self.session.expired(now_ms)
    }

    /// Queue `payload` on `channel`. Returns the sequence number assigned.
    pub fn send(
        &mut self,
        channel: ChannelId,
        stream: u32,
        fin: bool,
        payload: Vec<u8>,
        now_ms: u64,
    ) -> Result<u32> {
        let spec = self
            .channels
            .get(channel)
            .ok_or_else(|| DarmError::Rejected(format!("channel {} is not mounted", channel.0)))?;
        if payload.len() > spec.max_payload {
            return Err(DarmError::LimitExceeded(format!(
                "payload of {} bytes exceeds {} on channel {}",
                payload.len(),
                spec.max_payload,
                spec.name
            )));
        }
        let class = spec.class;
        let expires_ms = spec.ttl_ms.map(|ttl| now_ms.saturating_add(ttl));
        let seq = self.next_seq.get(&channel).copied().unwrap_or(0);
        let envelope = Envelope {
            channel,
            flags: if fin { FLAG_FIN } else { 0 },
            stream,
            seq,
            payload,
        };
        self.scheduler.push(class, envelope.encode(), expires_ms)?;
        // Only a queued message consumes a sequence number.
        self.next_seq.insert(channel, seq.wrapping_add(1));
        Ok(seq)
    }

    /// Sealed frames for the next queued message, highest priority first.
    /// `None` when the queues are empty.
    pub fn poll_outbound(&mut self, now_ms: u64) -> Result<Option<Vec<Vec<u8>>>> {
        match self.scheduler.pop(now_ms) {
            Some(encoded) => self.session.seal(&encoded, now_ms).map(Some),
            None => Ok(None),
        }
    }

    /// Open one frame from the carrier. `Ok(None)` means the frame was a
    /// fragment of a message that is not complete yet.
    pub fn on_frame(&mut self, frame: &[u8], now_ms: u64) -> Result<Option<Inbound>> {
        let message = match self.session.open(frame, now_ms) {
            Ok(Some(message)) => message,
            Ok(None) => return Ok(None),
            Err(error) => {
                self.stats.rejected_frames += 1;
                return Err(error);
            }
        };
        let envelope = Envelope::decode(&message).inspect_err(|_| {
            self.stats.rejected_frames += 1;
        })?;
        let Some(spec) = self.channels.get(envelope.channel) else {
            self.stats.unknown_channel += 1;
            return Err(DarmError::Rejected(format!(
                "channel {} is not mounted",
                envelope.channel.0
            )));
        };
        if envelope.payload.len() > spec.max_payload {
            self.stats.oversized += 1;
            return Err(DarmError::LimitExceeded(format!(
                "inbound payload exceeds limit on channel {}",
                spec.name
            )));
        }
        let bucket = self.buckets.entry(envelope.channel).or_insert(Bucket {
            tokens_milli: u64::from(spec.burst) * 1_000,
            updated_ms: now_ms,
        });
        if !bucket.take(spec, now_ms) {
            self.stats.rate_limited += 1;
            return Err(DarmError::LimitExceeded(format!(
                "rate limit on channel {}",
                spec.name
            )));
        }
        self.stats.delivered += 1;
        Ok(Some(Inbound {
            peer: self.session.peer(),
            channel: envelope.channel,
            stream: envelope.stream,
            seq: envelope.seq,
            fin: envelope.fin(),
            payload: envelope.payload,
        }))
    }
}
