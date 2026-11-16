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

    /// Whether outcome reports are inferred from conversations: the
    /// configured choice, else on unless this is a public mesh.
    pub fn auto_outcome_enabled(&self) -> bool {
        self.config.auto_outcome.unwrap_or(!self.public_mesh)
    }

    /// Before a turn: judge the previous answer by the user's follow-up.
    pub fn auto_follow_up(
        &mut self,
        namespace: &str,
        previous_answer: &str,
        follow_up: &str,
        now_ms: u64,
    ) {
        if !self.auto_outcome_enabled() {
            return;
        }
        let Some(verdict) = self
            .auto
            .on_follow_up(namespace, previous_answer, follow_up, now_ms)
        else {
            return;
        };
        for id in &verdict.memories {
            // Fails when the memory is gone or its recall was already
            // scored by hand; neither is an error here.
            if self.memory_outcome(id, verdict.helped, now_ms).is_ok() {
                if verdict.helped {
                    self.auto.reported_helped += 1;
                } else {
                    self.auto.reported_hurt += 1;
                }
            }
        }
    }

    /// After a turn: hold the recalled memories the answer drew on.
    pub fn auto_after_turn(
        &mut self,
        namespace: &str,
        query: &str,
        answer: &str,
        recalled: &[(Hash32, String)],
        now_ms: u64,
    ) {
        if !self.auto_outcome_enabled() {
            return;
        }
        let used = recalled
            .iter()
            .filter(|(_, content)| crate::auto_outcome::grounded(query, content, answer))
            .map(|(id, _)| *id)
            .collect();
        self.auto.after_turn(namespace, query, answer, used, now_ms);
    }

    /// Memory activity after sequence number `after`, with the newest
    /// sequence number so a reader knows where to resume.
    pub fn memory_feed(&self, after: u64, limit: usize) -> (u64, Vec<FeedEvent>) {
        let feed = self.memory.feed();
        (feed.head(), feed.since(after, limit))
    }
}
