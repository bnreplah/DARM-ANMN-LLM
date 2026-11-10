//! Learning from outcomes: was a recalled memory actually useful?
//!
//! Recall alone is a poor teacher. A memory that is retrieved often gets
//! rehearsed and consolidated whether or not it helped, so a confidently
//! wrong memory can become one of the strongest in the store. This module
//! tracks what happened *after* a recall: the caller reports whether the
//! memory helped or hurt, and that record, not the recall count, decides
//! how the memory is ranked and whether it may be consolidated.
//!
//! The estimate is the mean of a Beta posterior with a Beta(2, 2) prior:
//! `(helped + 2) / (helped + hurt + 4)`. It starts at 0.5 and needs several
//! observations to move far, so one report cannot make or break a memory.
//!
//! Checks and balances:
//!
//! - Feedback is only accepted for a memory with a recall that has not been
//!   scored yet, one report per recall. A memory cannot be boosted without
//!   having been used, and one use cannot be counted many times.
//! - A memory with more hurt than helped reports is not consolidated by
//!   recall count. One that is clearly harmful (at least
//!   [`MIN_OBSERVATIONS`] reports and an estimate below
//!   [`HARMFUL_BELOW`]) is demoted to short-term and left out of recall
//!   unless the caller asks for it.
//! - Outcomes are this node's own experience. They are stored locally and
//!   never gossiped: a peer's claim that a memory "helped" would be an easy
//!   way to promote poisoned memories.
//! - Nothing here deletes a memory or changes its signed content. Demotion
//!   lets ordinary decay and eviction do the forgetting.

use darm_core::{DarmError, Result};
use serde::{Deserialize, Serialize};

/// Reports needed before a memory can be judged harmful.
pub const MIN_OBSERVATIONS: u32 = 4;
/// Estimate below which a sufficiently observed memory is harmful.
pub const HARMFUL_BELOW: f32 = 0.35;
/// Recalls that may wait for feedback at once.
const MAX_PENDING: u32 = 8;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Outcome {
    Helped,
    Hurt,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Usefulness {
    helped: u32,
    hurt: u32,
    /// Recalls not yet scored.
    pending: u32,
}

impl Usefulness {
    pub fn observations(&self) -> u32 {
        self.helped.saturating_add(self.hurt)
    }

    /// Estimated probability that recalling this memory helps, in `(0, 1)`.
    pub fn estimate(&self) -> f32 {
        (self.helped as f32 + 2.0) / (self.observations() as f32 + 4.0)
    }

    /// Ranking multiplier in `(0.5, 1.5)`; exactly 1.0 with no reports.
    pub fn factor(&self) -> f32 {
        0.5 + self.estimate()
    }

    pub fn net_negative(&self) -> bool {
        self.hurt > self.helped
    }

    pub fn harmful(&self) -> bool {
        self.observations() >= MIN_OBSERVATIONS && self.estimate() < HARMFUL_BELOW
    }

    pub(crate) fn note_recall(&mut self) {
        self.pending = (self.pending + 1).min(MAX_PENDING);
    }

    /// Score one unscored recall.
    pub(crate) fn report(&mut self, outcome: Outcome) -> Result<()> {
        if self.pending == 0 {
            return Err(DarmError::Rejected(
                "no unscored recall of this memory".into(),
            ));
        }
        self.pending -= 1;
        self.replay(outcome);
        Ok(())
    }

    /// Apply a report read back from the log.
    pub(crate) fn replay(&mut self, outcome: Outcome) {
        match outcome {
            Outcome::Helped => self.helped = self.helped.saturating_add(1),
            Outcome::Hurt => self.hurt = self.hurt.saturating_add(1),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn after(helped: u32, hurt: u32) -> Usefulness {
        let mut u = Usefulness::default();
        for _ in 0..helped {
            u.replay(Outcome::Helped);
        }
        for _ in 0..hurt {
            u.replay(Outcome::Hurt);
        }
        u
    }

    #[test]
    fn unknown_memories_are_neutral() {
        let u = Usefulness::default();
        assert_eq!(u.estimate(), 0.5);
        assert_eq!(u.factor(), 1.0);
        assert!(!u.harmful() && !u.net_negative());
    }

    #[test]
    fn one_report_moves_the_estimate_only_a_little() {
        assert!((after(1, 0).estimate() - 0.6).abs() < 1e-6);
        assert!((after(0, 1).estimate() - 0.4).abs() < 1e-6);
        assert!(!after(0, 1).harmful(), "one bad report is not a verdict");
        assert!(!after(0, 3).harmful(), "nor are three");
        assert!(after(0, 4).harmful());
        assert!(after(1, 6).harmful());
        assert!(!after(3, 4).harmful(), "mixed evidence is not harmful");
        assert!(after(3, 4).net_negative());
    }

    #[test]
    fn the_factor_stays_within_its_bounds() {
        assert!(after(10_000, 0).factor() < 1.5);
        assert!(after(0, 10_000).factor() > 0.5);
        assert!(after(8, 2).factor() > 1.0);
    }

    #[test]
    fn feedback_needs_an_unscored_recall_and_counts_once() {
        let mut u = Usefulness::default();
        assert!(u.report(Outcome::Helped).is_err());
        u.note_recall();
        u.report(Outcome::Helped).unwrap();
        assert!(u.report(Outcome::Helped).is_err(), "one report per recall");
        assert_eq!(u.observations(), 1);
        // Unscored recalls are capped, so they cannot be banked without end.
        for _ in 0..100 {
            u.note_recall();
        }
        let mut accepted = 0;
        while u.report(Outcome::Hurt).is_ok() {
            accepted += 1;
        }
        assert_eq!(accepted, MAX_PENDING);
    }
}
