use super::locking::{Vote, VoteKind, vote_message};
use crate::canonical::Canonical;
use crate::{DarmError, Hash32, NodeId, NodeIdentity, Result, Signature, merkle};
use serde::{Deserialize, Serialize};

/// Longest `role` / `kind` label accepted in an entry.
pub const MAX_LABEL_BYTES: usize = 64;
/// Largest reputation change one report may carry (1.0 reputation unit).
pub const MAX_REPUTATION_DELTA_MILLI: i32 = 1_000;
/// Most shards one pooled compute job may commit.
pub const MAX_COMPUTE_SHARDS: u32 = 65_536;
/// Most workers one settlement may pay.
pub const MAX_SETTLE_PAYEES: usize = 256;
/// Largest single credit amount (keeps sums far from `u64` overflow).
pub const MAX_CREDIT_AMOUNT: u64 = 1_000_000_000_000;

/// One payment inside a [`EntryBody::ComputeSettle`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Payment {
    pub worker: NodeId,
    pub amount: u64,
}

/// Hash a validator proposal must carry as its payload to authorise a grant.
pub fn credit_grant_payload(to: &NodeId, amount: u64) -> Hash32 {
    let mut encoder = Canonical::new("credit-grant");
    encoder.fixed32(to.as_bytes()).u64(amount);
    Hash32::digest("credit-grant", encoder.as_bytes())
}

/// Reputation a validator loses for each recorded equivocation.
pub const EQUIVOCATION_PENALTY_MILLI: i64 = 1_000;

fn encode_vote(encoder: &mut Canonical, vote: &Vote) {
    let Signature::Ed25519(signature) = &vote.signature;
    encoder
        .u8(match vote.kind {
            VoteKind::Prevote => 1,
            VoteKind::Precommit => 2,
        })
        .u64(vote.height)
        .u32(vote.round)
        .opt_fixed32(vote.block.as_ref().map(Hash32::as_bytes))
        .fixed32(vote.validator.as_bytes())
        .bytes(signature);
}

/// Check that two votes prove equivocation on `chain`: same validator, kind,
/// height and round, different blocks, both correctly signed.
pub fn verify_equivocation(chain: &Hash32, first: &Vote, second: &Vote) -> Result<()> {
    for vote in [first, second] {
        let message = vote_message(
            chain,
            vote.kind,
            vote.height,
            vote.round,
            vote.block.as_ref(),
        );
        vote.validator.verify(&message, &vote.signature)?;
    }
    Ok(())
}

/// Proposal `kind` that authorises credit grants.
pub const CREDIT_GRANT_KIND: &str = "credit-grant";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum EntryBody {
    /// A node announces itself (biomimetic "birth" record).
    NodeRegistration {
        role: String,
        genome: Hash32,
        generation: u32,
        parent: Option<NodeId>,
    },
    /// Commit a memory record's content id. The content itself travels
    /// off-ledger (mesh gossip, IPFS, the archive server); the ledger only
    /// fixes who wrote what and when.
    MemoryCommit {
        memory: Hash32,
        importance_milli: u16,
    },
    /// Withdraw a memory. Only the committing author may revoke.
    MemoryRevoke { memory: Hash32 },
    /// Peer feedback. A node can never report on itself.
    ReputationReport {
        subject: NodeId,
        delta_milli: i32,
        evidence: Hash32,
    },
    /// Governance proposal (e.g. replication, validator changes). `payload`
    /// is the hash of the off-ledger proposal document.
    Proposal {
        kind: String,
        payload: Hash32,
        deadline_ms: u64,
    },
    /// Vote on a proposal, referenced by the proposal entry's id.
    Vote { proposal: Hash32, support: bool },
    /// Proof-of-useful-work record for a pooled compute job (CPU, GPU or
    /// both). The coordinator commits the Merkle root of the shard outputs
    /// that a majority of independent workers agreed on, and the root of the
    /// signed worker receipts behind it. Receipts and outputs stay off-ledger;
    /// anyone holding them can check them against these roots.
    ComputeCommit {
        job: Hash32,
        result_root: Hash32,
        receipts_root: Hash32,
        shards: u32,
        workers: u16,
    },
    /// Issue closed-loop compute credits. Author must be a validator; the
    /// referenced proposal must be `kind = "credit-grant"`, have passed the
    /// two-thirds validator vote, and carry `credit_grant_payload(to, amount)`.
    /// One grant per proposal. Credits cannot be transferred or redeemed;
    /// this is the only way supply is created.
    CreditGrant {
        proposal: Hash32,
        to: NodeId,
        amount: u64,
    },
    /// Pay the workers of a committed compute job out of the author's own
    /// balance. Author must be the job's coordinator; `receipts_root` must
    /// match the commit. Supply is conserved: credits only move.
    ComputeSettle {
        job: Hash32,
        receipts_root: Hash32,
        payments: Vec<Payment>,
    },
    /// Proof that a validator signed two different votes of one kind in one
    /// round. Anyone may submit it; the signatures are the proof. `first`
    /// is the vote whose block sorts lower (nil first), so each offence has
    /// exactly one valid encoding.
    Equivocation { first: Vote, second: Vote },
}

impl EntryBody {
    /// The feature that must be active for this entry to be valid, if any.
    pub fn feature(&self) -> Option<super::Feature> {
        match self {
            Self::CreditGrant { .. } | Self::ComputeSettle { .. } => Some(super::Feature::Credits),
            Self::Equivocation { .. } => Some(super::Feature::Evidence),
            _ => None,
        }
    }

    fn encode(&self, encoder: &mut Canonical) {
        match self {
            Self::NodeRegistration {
                role,
                genome,
                generation,
                parent,
            } => {
                encoder
                    .u8(1)
                    .str(role)
                    .fixed32(genome.as_bytes())
                    .u32(*generation)
                    .opt_fixed32(parent.as_ref().map(NodeId::as_bytes));
            }
            Self::MemoryCommit {
                memory,
                importance_milli,
            } => {
                encoder
                    .u8(2)
                    .fixed32(memory.as_bytes())
                    .u16(*importance_milli);
            }
            Self::MemoryRevoke { memory } => {
                encoder.u8(3).fixed32(memory.as_bytes());
            }
            Self::ReputationReport {
                subject,
                delta_milli,
                evidence,
            } => {
                encoder
                    .u8(4)
                    .fixed32(subject.as_bytes())
                    .i32(*delta_milli)
                    .fixed32(evidence.as_bytes());
            }
            Self::Proposal {
                kind,
                payload,
                deadline_ms,
            } => {
                encoder
                    .u8(5)
                    .str(kind)
                    .fixed32(payload.as_bytes())
                    .u64(*deadline_ms);
            }
            Self::Vote { proposal, support } => {
                encoder.u8(6).fixed32(proposal.as_bytes()).bool(*support);
            }
            Self::ComputeCommit {
                job,
                result_root,
                receipts_root,
                shards,
                workers,
            } => {
                encoder
                    .u8(7)
                    .fixed32(job.as_bytes())
                    .fixed32(result_root.as_bytes())
                    .fixed32(receipts_root.as_bytes())
                    .u32(*shards)
                    .u16(*workers);
            }
            Self::CreditGrant {
                proposal,
                to,
                amount,
            } => {
                encoder
                    .u8(8)
                    .fixed32(proposal.as_bytes())
                    .fixed32(to.as_bytes())
                    .u64(*amount);
            }
            Self::ComputeSettle {
                job,
                receipts_root,
                payments,
            } => {
                encoder
                    .u8(9)
                    .fixed32(job.as_bytes())
                    .fixed32(receipts_root.as_bytes())
                    .u32(payments.len() as u32);
                for payment in payments {
                    encoder
                        .fixed32(payment.worker.as_bytes())
                        .u64(payment.amount);
                }
            }
            Self::Equivocation { first, second } => {
                encoder.u8(10);
                encode_vote(encoder, first);
                encode_vote(encoder, second);
            }
        }
    }

    /// Stateless shape checks. State-dependent rules live in `LedgerState`.
    pub fn validate_shape(&self, author: &NodeId) -> Result<()> {
        match self {
            Self::NodeRegistration { role, .. } => check_label("role", role),
            Self::MemoryCommit {
                importance_milli, ..
            } => {
                if *importance_milli > 1_000 {
                    return Err(DarmError::malformed("entry", "importance_milli > 1000"));
                }
                Ok(())
            }
            Self::MemoryRevoke { .. } | Self::Vote { .. } => Ok(()),
            Self::ReputationReport {
                subject,
                delta_milli,
                ..
            } => {
                if subject == author {
                    return Err(DarmError::Rejected("self reputation report".into()));
                }
                if delta_milli.unsigned_abs() > MAX_REPUTATION_DELTA_MILLI.unsigned_abs() {
                    return Err(DarmError::malformed(
                        "entry",
                        "reputation delta out of range",
                    ));
                }
                Ok(())
            }
            Self::Proposal { kind, .. } => check_label("kind", kind),
            Self::ComputeCommit {
                shards, workers, ..
            } => {
                if *shards == 0 || *shards > MAX_COMPUTE_SHARDS {
                    return Err(DarmError::malformed("entry", "shards out of range"));
                }
                if *workers == 0 {
                    return Err(DarmError::malformed("entry", "compute needs a worker"));
                }
                Ok(())
            }
            Self::CreditGrant { amount, .. } => {
                if *amount == 0 || *amount > MAX_CREDIT_AMOUNT {
                    return Err(DarmError::malformed("entry", "grant amount out of range"));
                }
                Ok(())
            }
            Self::ComputeSettle { payments, .. } => {
                if payments.is_empty() || payments.len() > MAX_SETTLE_PAYEES {
                    return Err(DarmError::malformed("entry", "payee count out of range"));
                }
                let mut seen = std::collections::BTreeSet::new();
                for payment in payments {
                    if payment.amount == 0 || payment.amount > MAX_CREDIT_AMOUNT {
                        return Err(DarmError::malformed("entry", "payment out of range"));
                    }
                    if payment.worker == *author {
                        return Err(DarmError::Rejected("cannot pay yourself".into()));
                    }
                    if !seen.insert(payment.worker) {
                        return Err(DarmError::malformed("entry", "duplicate payee"));
                    }
                }
                Ok(())
            }
            Self::Equivocation { first, second } => {
                if first.validator != second.validator
                    || first.kind != second.kind
                    || first.height != second.height
                    || first.round != second.round
                {
                    return Err(DarmError::malformed(
                        "entry",
                        "votes are not from one validator, kind, height and round",
                    ));
                }
                if first.block >= second.block {
                    return Err(DarmError::malformed(
                        "entry",
                        "votes must differ and be in canonical order",
                    ));
                }
                Ok(())
            }
        }
    }
}

fn check_label(what: &'static str, value: &str) -> Result<()> {
    if value.is_empty() || value.len() > MAX_LABEL_BYTES {
        return Err(DarmError::malformed(what, "must be 1..=64 bytes"));
    }
    if !value
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b'.'))
    {
        return Err(DarmError::malformed(
            what,
            "allowed characters: [A-Za-z0-9_.-]",
        ));
    }
    Ok(())
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LedgerEntry {
    pub chain: Hash32,
    pub author: NodeId,
    pub timestamp_ms: u64,
    /// Random per-entry value so two identical bodies (e.g. a repeated vote
    /// attempt) still get distinct ids and duplicate detection is by id.
    pub nonce: u64,
    pub body: EntryBody,
}

impl LedgerEntry {
    pub fn canonical(&self) -> Vec<u8> {
        let mut encoder = Canonical::new("ledger-entry");
        encoder
            .fixed32(self.chain.as_bytes())
            .fixed32(self.author.as_bytes())
            .u64(self.timestamp_ms)
            .u64(self.nonce);
        self.body.encode(&mut encoder);
        encoder.finish()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SignedEntry {
    pub entry: LedgerEntry,
    pub signature: Signature,
}

impl SignedEntry {
    pub fn sign(
        identity: &NodeIdentity,
        chain: Hash32,
        body: EntryBody,
        timestamp_ms: u64,
    ) -> Result<Self> {
        let mut nonce = [0u8; 8];
        getrandom::fill(&mut nonce).map_err(|e| DarmError::Entropy(e.to_string()))?;
        let entry = LedgerEntry {
            chain,
            author: identity.node_id(),
            timestamp_ms,
            nonce: u64::from_be_bytes(nonce),
            body,
        };
        entry.body.validate_shape(&entry.author)?;
        let signature = identity.sign(&entry.canonical());
        Ok(Self { entry, signature })
    }

    pub fn id(&self) -> Hash32 {
        Hash32::digest("entry-id", &self.entry.canonical())
    }

    pub fn verify(&self, chain: &Hash32) -> Result<()> {
        if &self.entry.chain != chain {
            return Err(DarmError::Rejected("entry is for another chain".into()));
        }
        self.entry.body.validate_shape(&self.entry.author)?;
        self.entry
            .author
            .verify(&self.entry.canonical(), &self.signature)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BlockHeader {
    pub chain: Hash32,
    pub height: u64,
    pub round: u32,
    pub prev: Hash32,
    pub entries_root: Hash32,
    pub entry_count: u32,
    pub timestamp_ms: u64,
    pub proposer: NodeId,
    pub validators: Hash32,
}

impl BlockHeader {
    pub fn hash(&self) -> Hash32 {
        let mut encoder = Canonical::new("block-header");
        encoder
            .fixed32(self.chain.as_bytes())
            .u64(self.height)
            .u32(self.round)
            .fixed32(self.prev.as_bytes())
            .fixed32(self.entries_root.as_bytes())
            .u32(self.entry_count)
            .u64(self.timestamp_ms)
            .fixed32(self.proposer.as_bytes())
            .fixed32(self.validators.as_bytes());
        Hash32::digest("block", encoder.as_bytes())
    }
}

pub fn block_proposal_message(chain: &Hash32, block_hash: &Hash32) -> Vec<u8> {
    let mut encoder = Canonical::new("block-proposal");
    encoder
        .fixed32(chain.as_bytes())
        .fixed32(block_hash.as_bytes());
    encoder.finish()
}

pub fn attestation_message(chain: &Hash32, height: u64, block_hash: &Hash32) -> Vec<u8> {
    let mut encoder = Canonical::new("attestation");
    encoder
        .fixed32(chain.as_bytes())
        .u64(height)
        .fixed32(block_hash.as_bytes());
    encoder.finish()
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Attestation {
    pub validator: NodeId,
    pub signature: Signature,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Block {
    pub header: BlockHeader,
    pub entries: Vec<SignedEntry>,
    pub proposer_signature: Signature,
    /// The commit certificate. With `commit_round` unset these are
    /// one-vote-per-height attestations; with it set they are precommits
    /// from that round (see [`super::locking`]).
    #[serde(default)]
    pub attestations: Vec<Attestation>,
    /// Round whose precommits finalized the block under lock-based BFT.
    /// Not part of the block hash: the same block can be committed in any
    /// round at or after the one it was proposed in.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub commit_round: Option<u32>,
}

impl Block {
    pub fn hash(&self) -> Hash32 {
        self.header.hash()
    }

    pub fn entry_ids(&self) -> Vec<Hash32> {
        self.entries.iter().map(SignedEntry::id).collect()
    }

    /// Inclusion proof for one entry, for light clients and on-chain anchors.
    pub fn prove_entry(&self, entry_id: &Hash32) -> Option<merkle::Proof> {
        let ids = self.entry_ids();
        let index = ids.iter().position(|id| id == entry_id)?;
        merkle::prove(&ids, index)
    }
}
