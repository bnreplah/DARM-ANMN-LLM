use super::entry::{CREDIT_GRANT_KIND, EntryBody, SignedEntry, credit_grant_payload};
use super::validators::ValidatorSet;
use crate::{DarmError, Hash32, NodeId, Result};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet, HashMap};

/// Reputation is kept in milli-units and clamped to `[0, 5.000]`, matching the
/// range the original proof of concept used for its biomimetic thresholds.
pub const REPUTATION_MIN_MILLI: i64 = 0;
pub const REPUTATION_MAX_MILLI: i64 = 5_000;
pub const REPUTATION_DEFAULT_MILLI: i64 = 1_000;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Registration {
    pub role: String,
    pub genome: Hash32,
    pub generation: u32,
    pub parent: Option<NodeId>,
    pub registered_ms: u64,
}

/// A finalized pooled-compute result (see `EntryBody::ComputeCommit`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ComputeRecord {
    pub coordinator: NodeId,
    pub result_root: Hash32,
    pub receipts_root: Hash32,
    pub shards: u32,
    pub workers: u16,
    pub committed_ms: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProposalOutcome {
    Pending,
    Passed,
    Rejected,
    Expired,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProposalState {
    pub author: NodeId,
    pub kind: String,
    pub payload: Hash32,
    pub deadline_ms: u64,
    pub yes: BTreeSet<NodeId>,
    pub no: BTreeSet<NodeId>,
}

impl ProposalState {
    /// Validator-weighted tally with the same >2/3 rule as block finality
    /// (the PoC's 66% consensus threshold, made exact).
    pub fn outcome(&self, validators: &ValidatorSet, now_ms: u64) -> ProposalOutcome {
        if validators.has_quorum(self.yes.iter()) {
            return ProposalOutcome::Passed;
        }
        let total = validators.total_weight();
        let no: u64 = self.no.iter().map(|n| validators.weight(n)).sum();
        // Passing is impossible once more than a third has voted no.
        if no * 3 >= total {
            return ProposalOutcome::Rejected;
        }
        if now_ms > self.deadline_ms {
            return ProposalOutcome::Expired;
        }
        ProposalOutcome::Pending
    }
}

/// State derived by replaying finalized entries. Deterministic: it depends
/// only on entry order and block timestamps, never on local clocks.
#[derive(Debug, Clone, Default)]
pub struct LedgerState {
    memory_authors: HashMap<Hash32, NodeId>,
    memory_importance: HashMap<Hash32, u16>,
    revoked: BTreeSet<Hash32>,
    reputation: BTreeMap<NodeId, i64>,
    proposals: HashMap<Hash32, ProposalState>,
    registrations: BTreeMap<NodeId, Registration>,
    compute: HashMap<(NodeId, Hash32), ComputeRecord>,
    credits: BTreeMap<NodeId, u64>,
    granted: BTreeSet<Hash32>,
    settled: BTreeSet<(NodeId, Hash32)>,
    issued: u64,
}

impl LedgerState {
    pub fn apply(
        &mut self,
        entry: &SignedEntry,
        block_timestamp_ms: u64,
        validators: &ValidatorSet,
    ) -> Result<()> {
        let author = entry.entry.author;
        match &entry.entry.body {
            EntryBody::NodeRegistration {
                role,
                genome,
                generation,
                parent,
            } => {
                if self.registrations.contains_key(&author) {
                    return Err(DarmError::Rejected("node already registered".into()));
                }
                if let Some(parent) = parent {
                    let Some(parent_reg) = self.registrations.get(parent) else {
                        return Err(DarmError::Rejected("parent is not registered".into()));
                    };
                    if *generation != parent_reg.generation.saturating_add(1) {
                        return Err(DarmError::Rejected("generation must be parent + 1".into()));
                    }
                } else if *generation != 0 {
                    return Err(DarmError::Rejected("root nodes are generation 0".into()));
                }
                self.registrations.insert(
                    author,
                    Registration {
                        role: role.clone(),
                        genome: *genome,
                        generation: *generation,
                        parent: *parent,
                        registered_ms: block_timestamp_ms,
                    },
                );
            }
            EntryBody::MemoryCommit {
                memory,
                importance_milli,
            } => {
                if self.memory_authors.contains_key(memory) {
                    return Err(DarmError::Duplicate);
                }
                self.memory_authors.insert(*memory, author);
                self.memory_importance.insert(*memory, *importance_milli);
            }
            EntryBody::MemoryRevoke { memory } => match self.memory_authors.get(memory) {
                Some(owner) if *owner == author => {
                    self.revoked.insert(*memory);
                }
                Some(_) => {
                    return Err(DarmError::Unauthorized(
                        "only the author may revoke a memory".into(),
                    ));
                }
                None => return Err(DarmError::Rejected("unknown memory".into())),
            },
            EntryBody::ReputationReport {
                subject,
                delta_milli,
                ..
            } => {
                // Only validators may move reputation; otherwise anyone could
                // mint identities and vote a target up or down.
                if !validators.contains(&author) {
                    return Err(DarmError::Unauthorized(
                        "reputation reports require a validator".into(),
                    ));
                }
                let current = self.reputation(subject);
                let next = (current + i64::from(*delta_milli))
                    .clamp(REPUTATION_MIN_MILLI, REPUTATION_MAX_MILLI);
                self.reputation.insert(*subject, next);
            }
            EntryBody::Proposal {
                kind,
                payload,
                deadline_ms,
            } => {
                if *deadline_ms <= block_timestamp_ms {
                    return Err(DarmError::Rejected(
                        "proposal deadline already passed".into(),
                    ));
                }
                self.proposals.insert(
                    entry.id(),
                    ProposalState {
                        author,
                        kind: kind.clone(),
                        payload: *payload,
                        deadline_ms: *deadline_ms,
                        yes: BTreeSet::new(),
                        no: BTreeSet::new(),
                    },
                );
            }
            EntryBody::Vote { proposal, support } => {
                if !validators.contains(&author) {
                    return Err(DarmError::Unauthorized("only validators vote".into()));
                }
                let Some(state) = self.proposals.get_mut(proposal) else {
                    return Err(DarmError::Rejected("unknown proposal".into()));
                };
                if block_timestamp_ms > state.deadline_ms {
                    return Err(DarmError::Rejected("voting closed".into()));
                }
                if state.yes.contains(&author) || state.no.contains(&author) {
                    return Err(DarmError::Duplicate);
                }
                if *support {
                    state.yes.insert(author);
                } else {
                    state.no.insert(author);
                }
            }
            EntryBody::ComputeCommit {
                job,
                result_root,
                receipts_root,
                shards,
                workers,
            } => {
                // Records are keyed by (coordinator, job). Job ids commit to
                // the coordinator, but the ledger cannot see the job spec, so
                // keying by job alone would let anyone who saw a commit in
                // gossip front-run it under their own name.
                if self.compute.contains_key(&(author, *job)) {
                    return Err(DarmError::Duplicate);
                }
                self.compute.insert(
                    (author, *job),
                    ComputeRecord {
                        coordinator: author,
                        result_root: *result_root,
                        receipts_root: *receipts_root,
                        shards: *shards,
                        workers: *workers,
                        committed_ms: block_timestamp_ms,
                    },
                );
            }
            EntryBody::CreditGrant {
                proposal,
                to,
                amount,
            } => {
                if !validators.contains(&author) {
                    return Err(DarmError::Unauthorized("only validators grant".into()));
                }
                let Some(state) = self.proposals.get(proposal) else {
                    return Err(DarmError::Rejected("unknown proposal".into()));
                };
                if state.kind != CREDIT_GRANT_KIND
                    || state.payload != credit_grant_payload(to, *amount)
                {
                    return Err(DarmError::Rejected(
                        "proposal does not authorise this grant".into(),
                    ));
                }
                if state.outcome(validators, block_timestamp_ms) != ProposalOutcome::Passed {
                    return Err(DarmError::Rejected("grant proposal has not passed".into()));
                }
                if !self.granted.insert(*proposal) {
                    return Err(DarmError::Duplicate);
                }
                let balance = self.credit_balance(to);
                let (Some(balance), Some(issued)) = (
                    balance.checked_add(*amount),
                    self.issued.checked_add(*amount),
                ) else {
                    self.granted.remove(proposal);
                    return Err(DarmError::LimitExceeded("credit supply".into()));
                };
                self.credits.insert(*to, balance);
                self.issued = issued;
            }
            EntryBody::ComputeSettle {
                job,
                receipts_root,
                payments,
            } => {
                let key = (author, *job);
                let Some(record) = self.compute.get(&key) else {
                    return Err(DarmError::Rejected(
                        "no committed job by this author".into(),
                    ));
                };
                if record.receipts_root != *receipts_root {
                    return Err(DarmError::Rejected("receipts root mismatch".into()));
                }
                if payments.len() > usize::from(record.workers) {
                    return Err(DarmError::Rejected("more payees than workers".into()));
                }
                if self.settled.contains(&key) {
                    return Err(DarmError::Duplicate);
                }
                if payments
                    .iter()
                    .any(|p| !self.registrations.contains_key(&p.worker))
                {
                    return Err(DarmError::Rejected("payee is not registered".into()));
                }
                let total: u64 = payments.iter().map(|p| p.amount).sum();
                if self.credit_balance(&author) < total {
                    return Err(DarmError::Rejected("insufficient credits".into()));
                }
                // Totals are bounded by the issued supply, which is checked at
                // grant time, so these additions cannot overflow; verify anyway
                // before mutating so a failure leaves state untouched.
                let mut next = Vec::with_capacity(payments.len());
                for payment in payments {
                    let Some(balance) = self
                        .credit_balance(&payment.worker)
                        .checked_add(payment.amount)
                    else {
                        return Err(DarmError::LimitExceeded("credit balance".into()));
                    };
                    next.push((payment.worker, balance));
                }
                self.settled.insert(key);
                *self.credits.entry(author).or_insert(0) -= total;
                for (worker, balance) in next {
                    self.credits.insert(worker, balance);
                }
            }
        }
        Ok(())
    }

    /// Spendable credits. Non-transferable: only [`EntryBody::ComputeSettle`]
    /// moves them, and only to workers of a committed job.
    pub fn credit_balance(&self, node: &NodeId) -> u64 {
        self.credits.get(node).copied().unwrap_or(0)
    }

    /// Total ever granted. Always equals the sum of all balances.
    pub fn credits_issued(&self) -> u64 {
        self.issued
    }

    pub fn credit_balances(&self) -> impl Iterator<Item = (&NodeId, &u64)> {
        self.credits.iter()
    }

    pub fn reputation(&self, node: &NodeId) -> i64 {
        self.reputation
            .get(node)
            .copied()
            .unwrap_or(REPUTATION_DEFAULT_MILLI)
    }

    pub fn memory_author(&self, memory: &Hash32) -> Option<NodeId> {
        self.memory_authors.get(memory).copied()
    }

    pub fn memory_importance_milli(&self, memory: &Hash32) -> Option<u16> {
        self.memory_importance.get(memory).copied()
    }

    pub fn is_committed(&self, memory: &Hash32) -> bool {
        self.memory_authors.contains_key(memory) && !self.revoked.contains(memory)
    }

    pub fn is_revoked(&self, memory: &Hash32) -> bool {
        self.revoked.contains(memory)
    }

    pub fn committed_memories(&self) -> usize {
        self.memory_authors.len() - self.revoked.len()
    }

    pub fn proposal(&self, id: &Hash32) -> Option<&ProposalState> {
        self.proposals.get(id)
    }

    pub fn proposals(&self) -> impl Iterator<Item = (&Hash32, &ProposalState)> {
        self.proposals.iter()
    }

    pub fn registration(&self, node: &NodeId) -> Option<&Registration> {
        self.registrations.get(node)
    }

    pub fn registrations(&self) -> impl Iterator<Item = (&NodeId, &Registration)> {
        self.registrations.iter()
    }

    pub fn reputations(&self) -> impl Iterator<Item = (&NodeId, &i64)> {
        self.reputation.iter()
    }

    pub fn compute_job(&self, coordinator: &NodeId, job: &Hash32) -> Option<&ComputeRecord> {
        self.compute.get(&(*coordinator, *job))
    }

    pub fn compute_jobs(&self) -> usize {
        self.compute.len()
    }
}
