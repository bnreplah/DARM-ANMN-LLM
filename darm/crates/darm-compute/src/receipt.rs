//! Signed work receipts and majority verification.
//!
//! A worker signs `(job, shard, input hash, output hash, device, timing)`.
//! The coordinator [`tally`]s receipts against its plan:
//!
//! - receipts with bad signatures, for another job, for a shard the worker
//!   was not assigned, or over a different input are discarded (so an
//!   outsider cannot vote on results);
//! - one receipt per (shard, worker) counts;
//! - an output is accepted when at least `⌈τ_c·n⌉` of the shard's `n`
//!   *assigned* replicas agree on it (τ_c > 1/2, default 2/3; the job's
//!   threshold is the CDCP admission threshold of Paper 1, applied to
//!   results). Anything short of quorum is an abstention — `disputed` or
//!   `missing` — never a guess (the ESE "third outcome", Paper 2 §5). Workers who signed a different output are dissenters, and
//!   their receipt ids are evidence validators can cite in a reputation
//!   report.
//!
//! When every shard is agreed, the tally yields the ledger `ComputeCommit`
//! body: proof that useful work was done and cross-checked, recorded on the
//! same quorum-finalized chain as memories.
//!
//! CDCP-style agreement is consensus, not truth (Volume 1 §45): it assumes
//! replicas fail independently. The planner places them
//! on distinct nodes; it cannot tell whether distinct nodes share an owner,
//! so high-value jobs should also restrict `trust` to known operators.

use crate::device::{Backend, DeviceKind};
use crate::pool::JobPlan;
use darm_core::canonical::Canonical;
use darm_core::ledger::EntryBody;
use darm_core::{DarmError, Hash32, NodeId, NodeIdentity, Result, Signature, merkle};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkReceipt {
    pub job: Hash32,
    pub shard: u32,
    pub worker: NodeId,
    pub input: Hash32,
    /// Hash of the workload's canonical output encoding.
    pub output: Hash32,
    pub device: DeviceKind,
    pub backend: Backend,
    /// Work units in the shard (tokens, vectors, samples).
    pub units: u32,
    pub elapsed_ms: u32,
    pub completed_ms: u64,
}

impl WorkReceipt {
    pub fn canonical(&self) -> Vec<u8> {
        let mut encoder = Canonical::new("compute-receipt");
        encoder
            .fixed32(self.job.as_bytes())
            .u32(self.shard)
            .fixed32(self.worker.as_bytes())
            .fixed32(self.input.as_bytes())
            .fixed32(self.output.as_bytes())
            .u8(self.device.code())
            .u8(self.backend.code())
            .u32(self.units)
            .u32(self.elapsed_ms)
            .u64(self.completed_ms);
        encoder.finish()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SignedReceipt {
    pub receipt: WorkReceipt,
    pub signature: Signature,
}

impl SignedReceipt {
    pub fn sign(identity: &NodeIdentity, receipt: WorkReceipt) -> Result<Self> {
        if receipt.worker != identity.node_id() {
            return Err(DarmError::Unauthorized("receipt for another worker".into()));
        }
        let signature = identity.sign(&receipt.canonical());
        Ok(Self { receipt, signature })
    }

    pub fn id(&self) -> Hash32 {
        Hash32::digest("compute-receipt-id", &self.receipt.canonical())
    }

    pub fn verify(&self) -> Result<()> {
        self.receipt
            .worker
            .verify(&self.receipt.canonical(), &self.signature)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum ShardVerdict {
    Agreed { output: Hash32, votes: usize },
    Disputed { outputs: Vec<(Hash32, usize)> },
    Missing { received: usize },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Dissent {
    pub worker: NodeId,
    pub shard: u32,
    pub receipt: Hash32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Tally {
    pub job: Hash32,
    pub shards: Vec<ShardVerdict>,
    pub dissenters: Vec<Dissent>,
    pub rejected: usize,
    pub complete: bool,
    pub result_root: Option<Hash32>,
    pub receipts_root: Hash32,
    pub workers: u16,
    /// Accepted receipts, for feeding measurements back into the pool.
    #[serde(skip)]
    pub accepted: Vec<SignedReceipt>,
}

impl Tally {
    /// The ledger entry recording this job, once every shard is agreed.
    pub fn commit_body(&self) -> Option<EntryBody> {
        Some(EntryBody::ComputeCommit {
            job: self.job,
            result_root: self.result_root?,
            receipts_root: self.receipts_root,
            shards: u32::try_from(self.shards.len()).ok()?,
            workers: self.workers,
        })
    }
}

pub fn tally(plan: &JobPlan, receipts: &[SignedReceipt]) -> Tally {
    let shards = plan.spec.inputs.len();
    let mut rejected = 0;
    // (shard, worker) -> receipt; first valid one wins.
    let mut accepted: BTreeMap<(u32, NodeId), SignedReceipt> = BTreeMap::new();
    for signed in receipts {
        let r = &signed.receipt;
        let valid = r.job == plan.job
            && (r.shard as usize) < shards
            && plan.spec.inputs[r.shard as usize] == r.input
            && plan.assigned(r.shard, &r.worker)
            && signed.verify().is_ok();
        if !valid {
            rejected += 1;
            continue;
        }
        accepted
            .entry((r.shard, r.worker))
            .or_insert_with(|| signed.clone());
    }
    let mut verdicts = Vec::with_capacity(shards);
    let mut dissenters = Vec::new();
    let mut outputs = Vec::with_capacity(shards);
    for shard in 0..shards as u32 {
        let mut votes: BTreeMap<Hash32, Vec<&SignedReceipt>> = BTreeMap::new();
        for ((_, _), signed) in
            accepted.range((shard, NodeId([0; 32]))..=(shard, NodeId([0xff; 32])))
        {
            votes.entry(signed.receipt.output).or_default().push(signed);
        }
        let received: usize = votes.values().map(Vec::len).sum();
        let quorum = plan.spec.threshold.quorum(plan.replicas(shard)).max(1) as usize;
        let winner = votes
            .iter()
            .find(|(_, v)| v.len() >= quorum)
            .map(|(output, v)| (*output, v.len()));
        match winner {
            Some((output, count)) => {
                for (other, signers) in &votes {
                    if *other != output {
                        dissenters.extend(signers.iter().map(|s| Dissent {
                            worker: s.receipt.worker,
                            shard,
                            receipt: s.id(),
                        }));
                    }
                }
                outputs.push(output);
                verdicts.push(ShardVerdict::Agreed {
                    output,
                    votes: count,
                });
            }
            None if votes.len() > 1 => verdicts.push(ShardVerdict::Disputed {
                outputs: votes.iter().map(|(o, v)| (*o, v.len())).collect(),
            }),
            None => verdicts.push(ShardVerdict::Missing { received }),
        }
    }
    let complete = outputs.len() == shards;
    let receipt_ids: Vec<Hash32> = accepted.values().map(SignedReceipt::id).collect();
    let workers: BTreeSet<NodeId> = accepted.keys().map(|(_, w)| *w).collect();
    Tally {
        job: plan.job,
        shards: verdicts,
        dissenters,
        rejected,
        complete,
        result_root: complete.then(|| merkle::root(&outputs)),
        receipts_root: merkle::root(&receipt_ids),
        workers: u16::try_from(workers.len()).unwrap_or(u16::MAX),
        accepted: accepted.into_values().collect(),
    }
}
