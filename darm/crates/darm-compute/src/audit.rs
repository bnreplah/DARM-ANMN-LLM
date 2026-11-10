//! Sampled audits of finished compute jobs.
//!
//! Redundancy verifies work by doing all of it several times. A sampled
//! audit verifies it by redoing a small, unpredictable part: after the
//! workers' receipts are fixed, a seed that nobody could know beforehand
//! selects which shards are re-run and by whom. A worker that returns a
//! wrong result on `k` shards is caught with probability `1 - (1 - p)^k`
//! at audit rate `p`, for roughly `p` extra work instead of 2x or more.
//!
//! This differs from the planner's spot checks ([`JobSpec::spot_checked`]),
//! which are fixed when the job is planned from values the workers may see.
//! Here the order is what matters:
//!
//! 1. Workers return signed receipts; the coordinator tallies them and
//!    publishes the tally's `receipts_root` (for example in the ledger).
//! 2. A **beacon** becomes known that was not predictable in step 1.
//! 3. Anyone can recompute [`plan_audit`]: the audited shards and their
//!    auditors follow from `(job, receipts_root, beacon)` alone, so neither
//!    the coordinator nor a worker chooses them.
//! 4. Auditors re-run their shards and sign ordinary receipts;
//!    [`audit_tally`] compares them with the accepted outputs.
//! 5. A contradiction is settled by more auditors ([`escalation`],
//!    [`resolve`]), never by taking either side's word.
//!
//! What this does and does not give you:
//!
//! - The beacon must be fixed after the receipts root is public and must
//!   not be chosen by a worker or the coordinator. The hash of a ledger
//!   block finalized after the root was recorded is a reasonable source,
//!   with a known limit: that block's proposer can try a few candidate
//!   blocks to bias the sample. A verifiable random function would remove
//!   that; this crate does not implement one.
//! - Auditors re-run whole shards, so the cost is the audit rate. Schemes
//!   that check a sampled slice of a model's hidden states are cheaper and
//!   are not attempted here.
//! - Detection deters only if being caught costs something. Today that is
//!   reputation ([`crate::pool`] trust); there is no stake to slash.
//! - Like redundancy, this is agreement, not truth: auditors that share an
//!   operator with the worker can confirm a wrong result.

use crate::pool::JobPlan;
use crate::receipt::{ShardVerdict, SignedReceipt, Tally};
use darm_core::canonical::Canonical;
use darm_core::{DarmError, Hash32, NodeId, Result};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// Largest number of auditors asked to settle one contradicted shard.
pub const MAX_ESCALATION: usize = 7;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct AuditPolicy {
    /// Shards audited, per mille. 50 audits about one shard in twenty.
    pub rate_milli: u16,
}

impl AuditPolicy {
    pub fn validate(&self) -> Result<()> {
        if self.rate_milli == 0 || self.rate_milli > 1_000 {
            return Err(DarmError::malformed("audit", "rate_milli must be 1..=1000"));
        }
        Ok(())
    }
}

/// Probability that at least one of `cheated` wrong shards is audited.
pub fn detection_probability(rate_milli: u16, cheated: u32) -> f64 {
    let miss = 1.0 - f64::from(rate_milli.min(1_000)) / 1_000.0;
    1.0 - miss.powi(i32::try_from(cheated).unwrap_or(i32::MAX))
}

fn audit_seed(job: &Hash32, receipts_root: &Hash32, beacon: &Hash32) -> Hash32 {
    let mut encoder = Canonical::new("compute-audit-seed");
    encoder
        .fixed32(job.as_bytes())
        .fixed32(receipts_root.as_bytes())
        .fixed32(beacon.as_bytes());
    Hash32::digest("compute-audit-seed", encoder.as_bytes())
}

fn shard_draw(seed: &Hash32, shard: u32) -> u64 {
    let mut encoder = Canonical::new("compute-audit-shard");
    encoder.fixed32(seed.as_bytes()).u32(shard);
    let digest = Hash32::digest("compute-audit-shard", encoder.as_bytes());
    let bytes = digest.as_bytes();
    u64::from_be_bytes([
        bytes[0], bytes[1], bytes[2], bytes[3], bytes[4], bytes[5], bytes[6], bytes[7],
    ])
}

/// Candidates ordered for `shard` by rendezvous hashing: every party with
/// the seed computes the same order, and no party can steer it.
fn ranked(seed: &Hash32, shard: u32, candidates: &[NodeId], exclude: &[NodeId]) -> Vec<NodeId> {
    let mut scored: Vec<(Hash32, NodeId)> = candidates
        .iter()
        .filter(|node| !exclude.contains(node))
        .map(|node| {
            let mut encoder = Canonical::new("compute-audit-pick");
            encoder
                .fixed32(seed.as_bytes())
                .u32(shard)
                .fixed32(node.as_bytes());
            (
                Hash32::digest("compute-audit-pick", encoder.as_bytes()),
                *node,
            )
        })
        .collect();
    scored.sort();
    scored.dedup_by_key(|(_, node)| *node);
    scored.into_iter().map(|(_, node)| node).collect()
}

fn workers_of(plan: &JobPlan, shard: u32) -> Vec<NodeId> {
    plan.assignments
        .iter()
        .filter(|a| a.shard == shard)
        .map(|a| a.worker)
        .collect()
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AuditAssignment {
    pub shard: u32,
    pub auditor: NodeId,
    /// The output the job accepted for this shard, which the auditor's
    /// result is compared with. The auditor is not told this value.
    #[serde(skip)]
    pub expected: Option<Hash32>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AuditPlan {
    pub job: Hash32,
    pub seed: Hash32,
    pub assignments: Vec<AuditAssignment>,
    /// Sampled shards with no eligible auditor (every candidate already
    /// worked on them). They stay unaudited and are reported, not hidden.
    pub unassignable: Vec<u32>,
}

/// Choose the shards to audit and one auditor for each.
///
/// `candidates` are the nodes able to run the workload; a shard's own
/// workers are never its auditors. Only a complete tally can be audited:
/// its `receipts_root` is what binds the sample to results that can no
/// longer change.
pub fn plan_audit(
    plan: &JobPlan,
    tally: &Tally,
    beacon: &Hash32,
    policy: AuditPolicy,
    candidates: &[NodeId],
) -> Result<AuditPlan> {
    policy.validate()?;
    if tally.job != plan.job {
        return Err(DarmError::Rejected("tally is for another job".into()));
    }
    if !tally.complete || tally.shards.len() != plan.spec.inputs.len() {
        return Err(DarmError::Rejected(
            "only a complete tally can be audited".into(),
        ));
    }
    let seed = audit_seed(&plan.job, &tally.receipts_root, beacon);
    let mut assignments = Vec::new();
    let mut unassignable = Vec::new();
    for (index, verdict) in tally.shards.iter().enumerate() {
        let shard = index as u32;
        if shard_draw(&seed, shard) % 1_000 >= u64::from(policy.rate_milli) {
            continue;
        }
        let ShardVerdict::Agreed { output, .. } = verdict else {
            return Err(DarmError::Rejected("tally shard is not agreed".into()));
        };
        match ranked(&seed, shard, candidates, &workers_of(plan, shard)).first() {
            Some(auditor) => assignments.push(AuditAssignment {
                shard,
                auditor: *auditor,
                expected: Some(*output),
            }),
            None => unassignable.push(shard),
        }
    }
    Ok(AuditPlan {
        job: plan.job,
        seed,
        assignments,
        unassignable,
    })
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum AuditVerdict {
    /// The auditor reproduced the accepted output.
    Confirmed,
    /// The auditor signed a different output. `receipt` is the evidence.
    Contradicted { output: Hash32, receipt: Hash32 },
    /// No valid receipt from the assigned auditor.
    Missing,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AuditOutcome {
    pub job: Hash32,
    pub verdicts: Vec<(u32, AuditVerdict)>,
    /// Receipts that were not from the assigned auditor, not over the
    /// shard's input, or not validly signed.
    pub rejected: usize,
    /// Every sampled shard was confirmed and none was unassignable.
    pub clean: bool,
}

impl AuditOutcome {
    pub fn contradicted(&self) -> impl Iterator<Item = u32> + '_ {
        self.verdicts
            .iter()
            .filter(|(_, v)| matches!(v, AuditVerdict::Contradicted { .. }))
            .map(|(shard, _)| *shard)
    }
}

fn valid_for(plan: &JobPlan, signed: &SignedReceipt) -> bool {
    let r = &signed.receipt;
    r.job == plan.job
        && plan.spec.inputs.get(r.shard as usize) == Some(&r.input)
        && signed.verify().is_ok()
}

/// Compare the auditors' receipts with the outputs the job accepted.
pub fn audit_tally(plan: &JobPlan, audit: &AuditPlan, receipts: &[SignedReceipt]) -> AuditOutcome {
    let mut by_slot: BTreeMap<(u32, NodeId), &SignedReceipt> = BTreeMap::new();
    let mut rejected = 0;
    for signed in receipts {
        let r = &signed.receipt;
        let assigned = audit
            .assignments
            .iter()
            .any(|a| a.shard == r.shard && a.auditor == r.worker);
        if audit.job != plan.job || !assigned || !valid_for(plan, signed) {
            rejected += 1;
            continue;
        }
        by_slot.entry((r.shard, r.worker)).or_insert(signed);
    }
    let verdicts: Vec<(u32, AuditVerdict)> = audit
        .assignments
        .iter()
        .map(|a| {
            let verdict = match (by_slot.get(&(a.shard, a.auditor)), a.expected) {
                (Some(signed), Some(expected)) if signed.receipt.output == expected => {
                    AuditVerdict::Confirmed
                }
                (Some(signed), Some(_)) => AuditVerdict::Contradicted {
                    output: signed.receipt.output,
                    receipt: signed.id(),
                },
                _ => AuditVerdict::Missing,
            };
            (a.shard, verdict)
        })
        .collect();
    let clean = audit.unassignable.is_empty()
        && verdicts.iter().all(|(_, v)| *v == AuditVerdict::Confirmed);
    AuditOutcome {
        job: plan.job,
        verdicts,
        rejected,
        clean,
    }
}

/// The next `count` auditors for a contradicted shard, in the same
/// seed-determined order, skipping its workers and its first auditor.
pub fn escalation(
    plan: &JobPlan,
    audit: &AuditPlan,
    shard: u32,
    candidates: &[NodeId],
    count: usize,
) -> Vec<NodeId> {
    let mut exclude = workers_of(plan, shard);
    exclude.extend(
        audit
            .assignments
            .iter()
            .filter(|a| a.shard == shard)
            .map(|a| a.auditor),
    );
    let mut order = ranked(&audit.seed, shard, candidates, &exclude);
    order.truncate(count.min(MAX_ESCALATION));
    order
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum Resolution {
    /// A majority of the escalation auditors reproduced the accepted
    /// output: the first auditor was wrong.
    Upheld,
    /// A majority reproduced a different output: the accepted result was
    /// wrong and the job's commit must not stand.
    Overturned { output: Hash32 },
    /// No output has a majority of the escalation auditors.
    Unresolved,
}

/// Settle a contradicted shard from the receipts of its `escalated`
/// auditors. A majority of those auditors (not of the receipts that
/// happened to arrive) is required, so silence cannot decide the outcome.
pub fn resolve(
    plan: &JobPlan,
    shard: u32,
    accepted: &Hash32,
    escalated: &[NodeId],
    receipts: &[SignedReceipt],
) -> Resolution {
    let mut votes: BTreeMap<NodeId, Hash32> = BTreeMap::new();
    for signed in receipts {
        let r = &signed.receipt;
        if r.shard == shard && escalated.contains(&r.worker) && valid_for(plan, signed) {
            votes.entry(r.worker).or_insert(r.output);
        }
    }
    let mut counts: BTreeMap<Hash32, usize> = BTreeMap::new();
    for output in votes.values() {
        *counts.entry(*output).or_default() += 1;
    }
    let majority = escalated.len() / 2 + 1;
    match counts.into_iter().find(|(_, n)| *n >= majority) {
        Some((output, _)) if output == *accepted => Resolution::Upheld,
        Some((output, _)) => Resolution::Overturned { output },
        None => Resolution::Unresolved,
    }
}

#[cfg(test)]
mod tests;
