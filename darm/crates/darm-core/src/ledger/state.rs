use super::entry::{EntryBody, SignedEntry};
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
        }
        Ok(())
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
}
