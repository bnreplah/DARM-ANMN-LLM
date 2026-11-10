//! The memory feed: a bounded, ordered record of what the store just did.
//!
//! Remembering, learning from a peer, recalling, scoring an outcome,
//! consolidating, demoting, finalizing and forgetting each append one event.
//! A reader polls with the last sequence number it saw and gets what
//! happened since, which is enough to show a live view of the memory at
//! work. The feed holds ids and event kinds only, never memory content, so
//! exposing it does not widen who can read a memory.
//!
//! It is kept in memory and bounded; it is an activity view, not an audit
//! log. The durable record is the store's own log and the ledger.

use darm_core::Hash32;
use serde::Serialize;
use std::collections::VecDeque;

pub const FEED_CAPACITY: usize = 512;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum FeedKind {
    /// Written by this node.
    Remembered,
    /// Received from a peer.
    Learned,
    Recalled,
    Helped,
    Hurt,
    /// Promoted to long-term.
    Consolidated,
    /// Returned to short-term after harmful outcomes.
    Demoted,
    /// Its commit was finalized on the ledger.
    Finalized,
    Forgotten,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct FeedEvent {
    /// Increases by one per event for the life of the store.
    pub seq: u64,
    pub at_ms: u64,
    pub kind: FeedKind,
    pub memory: Hash32,
}

#[derive(Debug, Default)]
pub struct Feed {
    events: VecDeque<FeedEvent>,
    next_seq: u64,
}

impl Feed {
    pub(crate) fn push(&mut self, kind: FeedKind, memory: Hash32, at_ms: u64) {
        self.next_seq += 1;
        if self.events.len() == FEED_CAPACITY {
            self.events.pop_front();
        }
        self.events.push_back(FeedEvent {
            seq: self.next_seq,
            at_ms,
            kind,
            memory,
        });
    }

    /// Up to `limit` events with `seq > after`, oldest first.
    pub fn since(&self, after: u64, limit: usize) -> Vec<FeedEvent> {
        self.events
            .iter()
            .filter(|event| event.seq > after)
            .take(limit)
            .copied()
            .collect()
    }

    /// Sequence number of the newest event, 0 when empty.
    pub fn head(&self) -> u64 {
        self.next_seq
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn readers_resume_from_their_last_sequence_number() {
        let mut feed = Feed::default();
        let id = Hash32::digest("feed-test", b"m");
        for at in 1..=5 {
            feed.push(FeedKind::Recalled, id, at);
        }
        assert_eq!(feed.head(), 5);
        let first = feed.since(0, 3);
        assert_eq!(first.iter().map(|e| e.seq).collect::<Vec<_>>(), [1, 2, 3]);
        let rest = feed.since(first[2].seq, 10);
        assert_eq!(rest.iter().map(|e| e.seq).collect::<Vec<_>>(), [4, 5]);
        assert!(feed.since(5, 10).is_empty());
    }

    #[test]
    fn the_feed_is_bounded_and_keeps_the_newest() {
        let mut feed = Feed::default();
        let id = Hash32::digest("feed-test", b"m");
        for at in 0..(FEED_CAPACITY as u64 + 40) {
            feed.push(FeedKind::Recalled, id, at);
        }
        let all = feed.since(0, usize::MAX);
        assert_eq!(all.len(), FEED_CAPACITY);
        assert_eq!(all[0].seq, 41);
        assert_eq!(all.last().unwrap().seq, feed.head());
    }
}
