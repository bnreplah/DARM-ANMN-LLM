//! Outbound scheduling across sync classes.
//!
//! Each class has its own bounded queue. `tight` messages always leave
//! first. The remaining classes share what is left by weight (pipeline 4,
//! block 2, async 1), so a flood of bulk gossip delays the ledger by at most
//! a bounded number of messages and is itself never starved.
//!
//! Queues are bounded in bytes. A full queue refuses new work instead of
//! growing: back-pressure reaches the workload, and memory use per link has a
//! hard ceiling. Messages with a deadline are dropped at dequeue time once
//! they are stale, because a late activation is worse than a missing one.

use crate::channel::SyncClass;
use darm_core::{DarmError, Result};
use std::collections::VecDeque;

/// Weighted share of each class once `tight` is empty, indexed by
/// [`SyncClass::index`]. `tight` is strict priority and has no weight.
const WEIGHTS: [u32; 3] = [1, 2, 4];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct QueueLimits {
    /// Byte budget per class, indexed by [`SyncClass::index`].
    pub max_bytes: [usize; 4],
}

impl Default for QueueLimits {
    fn default() -> Self {
        Self {
            max_bytes: [8 << 20, 4 << 20, 4 << 20, 2 << 20],
        }
    }
}

#[derive(Debug)]
struct Queued {
    bytes: Vec<u8>,
    expires_ms: Option<u64>,
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct SchedulerStats {
    pub enqueued: u64,
    pub dequeued: u64,
    pub refused_full: u64,
    pub expired: u64,
}

#[derive(Debug)]
pub struct Scheduler {
    queues: [VecDeque<Queued>; 4],
    bytes: [usize; 4],
    credits: [u32; 3],
    limits: QueueLimits,
    pub stats: SchedulerStats,
}

impl Scheduler {
    pub fn new(limits: QueueLimits) -> Self {
        Self {
            queues: Default::default(),
            bytes: [0; 4],
            credits: WEIGHTS,
            limits,
            stats: SchedulerStats::default(),
        }
    }

    /// Queue an encoded envelope. Fails when the class budget is spent.
    pub fn push(
        &mut self,
        class: SyncClass,
        bytes: Vec<u8>,
        expires_ms: Option<u64>,
    ) -> Result<()> {
        let index = class.index();
        if self.bytes[index].saturating_add(bytes.len()) > self.limits.max_bytes[index] {
            self.stats.refused_full += 1;
            return Err(DarmError::LimitExceeded(format!(
                "{class:?} outbound queue is full"
            )));
        }
        self.bytes[index] += bytes.len();
        self.queues[index].push_back(Queued { bytes, expires_ms });
        self.stats.enqueued += 1;
        Ok(())
    }

    /// The next encoded envelope to send, or `None` when nothing is ready.
    pub fn pop(&mut self, now_ms: u64) -> Option<Vec<u8>> {
        if let Some(bytes) = self.pop_live(SyncClass::Tight.index(), now_ms) {
            return Some(bytes);
        }
        // Two passes: the second runs after credits refill, so a class that
        // had used up its share still gets served when it is the only one
        // with work.
        for _ in 0..2 {
            for index in (0..WEIGHTS.len()).rev() {
                if self.credits[index] == 0 {
                    continue;
                }
                if let Some(bytes) = self.pop_live(index, now_ms) {
                    self.credits[index] -= 1;
                    return Some(bytes);
                }
            }
            self.credits = WEIGHTS;
        }
        None
    }

    fn pop_live(&mut self, index: usize, now_ms: u64) -> Option<Vec<u8>> {
        while let Some(item) = self.queues[index].pop_front() {
            self.bytes[index] -= item.bytes.len();
            if item.expires_ms.is_some_and(|deadline| now_ms >= deadline) {
                self.stats.expired += 1;
                continue;
            }
            self.stats.dequeued += 1;
            return Some(item.bytes);
        }
        None
    }

    pub fn queued_bytes(&self, class: SyncClass) -> usize {
        self.bytes[class.index()]
    }

    /// Bytes that can still be queued for `class`.
    pub fn free_bytes(&self, class: SyncClass) -> usize {
        self.limits.max_bytes[class.index()].saturating_sub(self.bytes[class.index()])
    }

    pub fn is_empty(&self) -> bool {
        self.queues.iter().all(VecDeque::is_empty)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tagged(tag: u8) -> Vec<u8> {
        vec![tag; 4]
    }

    #[test]
    fn tight_always_leaves_first() {
        let mut scheduler = Scheduler::new(QueueLimits::default());
        for _ in 0..50 {
            scheduler.push(SyncClass::Async, tagged(0), None).unwrap();
        }
        scheduler.push(SyncClass::Tight, tagged(3), None).unwrap();
        assert_eq!(scheduler.pop(0).unwrap(), tagged(3));
    }

    #[test]
    fn other_classes_share_by_weight_without_starvation() {
        let mut scheduler = Scheduler::new(QueueLimits::default());
        for _ in 0..70 {
            scheduler.push(SyncClass::Async, tagged(0), None).unwrap();
            scheduler.push(SyncClass::Block, tagged(1), None).unwrap();
            scheduler
                .push(SyncClass::Pipeline, tagged(2), None)
                .unwrap();
        }
        let mut counts = [0u32; 3];
        for _ in 0..70 {
            counts[usize::from(scheduler.pop(0).unwrap()[0])] += 1;
        }
        assert_eq!(
            counts,
            [10, 20, 40],
            "one round is 4 pipeline, 2 block, 1 async"
        );
    }

    #[test]
    fn a_lone_class_is_served_past_its_credit() {
        let mut scheduler = Scheduler::new(QueueLimits::default());
        for _ in 0..10 {
            scheduler.push(SyncClass::Async, tagged(0), None).unwrap();
        }
        for _ in 0..10 {
            assert!(scheduler.pop(0).is_some());
        }
        assert!(scheduler.pop(0).is_none());
        assert!(scheduler.is_empty());
    }

    #[test]
    fn full_queue_refuses_and_stale_messages_expire() {
        let mut limits = QueueLimits::default();
        limits.max_bytes[SyncClass::Tight.index()] = 8;
        let mut scheduler = Scheduler::new(limits);
        scheduler
            .push(SyncClass::Tight, tagged(1), Some(100))
            .unwrap();
        scheduler
            .push(SyncClass::Tight, tagged(2), Some(300))
            .unwrap();
        assert!(scheduler.push(SyncClass::Tight, tagged(3), None).is_err());
        assert_eq!(scheduler.stats.refused_full, 1);
        // At t=200 the first message is stale and is skipped.
        assert_eq!(scheduler.pop(200).unwrap(), tagged(2));
        assert_eq!(scheduler.stats.expired, 1);
        assert_eq!(scheduler.queued_bytes(SyncClass::Tight), 0);
    }
}
