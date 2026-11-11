//! Learning from recall outcomes and the memory activity feed.
//!
//! Both are local to this node. Outcome reports are this node's own
//! experience and are never gossiped; the feed exposes ids and event kinds,
//! never memory content.

use super::DarmNode;
use darm_core::{Hash32, Result};
use darm_memory::{FeedEvent, Outcome};

impl DarmNode {
    /// Report whether a recalled memory helped. Local learning only: the
    /// report is stored on this node and never gossiped.
    pub fn memory_outcome(&mut self, id: &Hash32, helped: bool, now_ms: u64) -> Result<f32> {
        let outcome = if helped {
            Outcome::Helped
        } else {
            Outcome::Hurt
        };
        self.memory.record_outcome(id, outcome, now_ms)
    }

    /// Memory activity after sequence number `after`, with the newest
    /// sequence number so a reader knows where to resume.
    pub fn memory_feed(&self, after: u64, limit: usize) -> (u64, Vec<FeedEvent>) {
        let feed = self.memory.feed();
        (feed.head(), feed.since(after, limit))
    }
}
