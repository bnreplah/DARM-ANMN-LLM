use crate::canonical::Canonical;
use crate::{DarmError, Hash32, NodeId, NodeIdentity, Result, Signature, merkle};
use serde::{Deserialize, Serialize};

/// Longest `role` / `kind` label accepted in an entry.
pub const MAX_LABEL_BYTES: usize = 64;
/// Largest reputation change one report may carry (1.0 reputation unit).
pub const MAX_REPUTATION_DELTA_MILLI: i32 = 1_000;
/// Most shards one pooled compute job may commit.
pub const MAX_COMPUTE_SHARDS: u32 = 65_536;

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
}

impl EntryBody {
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
    #[serde(default)]
    pub attestations: Vec<Attestation>,
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
