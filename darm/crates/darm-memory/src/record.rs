use darm_core::canonical::Canonical;
use darm_core::{DarmError, Hash32, NodeId, NodeIdentity, Result, Signature};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MemoryKind {
    /// Something that happened (a conversation turn, an observation).
    Episodic,
    /// A distilled fact or insight.
    Semantic,
    /// How to do something (a procedure, a tool recipe).
    Procedural,
    /// Emotional context, the PoC's EmotionalNode output.
    Affective,
}

impl MemoryKind {
    fn tag(self) -> u8 {
        match self {
            Self::Episodic => 1,
            Self::Semantic => 2,
            Self::Procedural => 3,
            Self::Affective => 4,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum Scope {
    /// Never gossiped, never committed to the ledger.
    #[default]
    Local,
    /// Shared with mesh peers.
    Mesh,
}

impl Scope {
    fn tag(self) -> u8 {
        match self {
            Self::Local => 1,
            Self::Mesh => 2,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MemoryRecord {
    pub author: NodeId,
    pub created_ms: u64,
    pub kind: MemoryKind,
    pub scope: Scope,
    /// 0..=1000
    pub importance_milli: u16,
    /// -1000..=1000, negative to positive affect.
    #[serde(default)]
    pub valence_milli: i16,
    #[serde(default)]
    pub tags: Vec<String>,
    pub content: String,
    /// Memory this one refines or answers, if any.
    #[serde(default)]
    pub parent: Option<Hash32>,
}

pub const MAX_CONTENT_BYTES: usize = 16 * 1024;
pub const MAX_TAGS: usize = 16;
pub const MAX_TAG_BYTES: usize = 48;

impl MemoryRecord {
    pub fn canonical(&self) -> Vec<u8> {
        let mut encoder = Canonical::new("memory-record");
        encoder
            .fixed32(self.author.as_bytes())
            .u64(self.created_ms)
            .u8(self.kind.tag())
            .u8(self.scope.tag())
            .u16(self.importance_milli)
            .i16(self.valence_milli)
            .str_list(&self.tags)
            .str(&self.content)
            .opt_fixed32(self.parent.as_ref().map(Hash32::as_bytes));
        encoder.finish()
    }

    pub fn id(&self) -> Hash32 {
        Hash32::digest("memory-record", &self.canonical())
    }

    pub fn validate(&self) -> Result<()> {
        if self.content.trim().is_empty() {
            return Err(DarmError::malformed("memory", "empty content"));
        }
        if self.content.len() > MAX_CONTENT_BYTES {
            return Err(DarmError::LimitExceeded(format!(
                "memory content exceeds {MAX_CONTENT_BYTES} bytes"
            )));
        }
        if self.importance_milli > 1_000 {
            return Err(DarmError::malformed("memory", "importance_milli > 1000"));
        }
        if !(-1_000..=1_000).contains(&self.valence_milli) {
            return Err(DarmError::malformed("memory", "valence_milli out of range"));
        }
        if self.tags.len() > MAX_TAGS {
            return Err(DarmError::LimitExceeded("too many tags".into()));
        }
        for tag in &self.tags {
            if tag.is_empty()
                || tag.len() > MAX_TAG_BYTES
                || !tag
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b'.' | b':'))
            {
                return Err(DarmError::malformed("memory", "invalid tag"));
            }
        }
        Ok(())
    }

    pub fn importance(&self) -> f32 {
        f32::from(self.importance_milli) / 1_000.0
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SignedMemory {
    pub record: MemoryRecord,
    pub signature: Signature,
}

impl SignedMemory {
    pub fn sign(identity: &NodeIdentity, mut record: MemoryRecord) -> Result<Self> {
        record.author = identity.node_id();
        record.validate()?;
        let signature = identity.sign(&record.canonical());
        Ok(Self { record, signature })
    }

    pub fn id(&self) -> Hash32 {
        self.record.id()
    }

    pub fn verify(&self) -> Result<()> {
        self.record.validate()?;
        self.record
            .author
            .verify(&self.record.canonical(), &self.signature)
    }
}

/// Author-signed deletion marker. Tombstones win over inserts, so a forgotten
/// memory cannot be resurrected by a peer that still has a copy.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SignedTombstone {
    pub memory: Hash32,
    pub author: NodeId,
    pub timestamp_ms: u64,
    pub signature: Signature,
}

impl SignedTombstone {
    fn message(memory: &Hash32, author: &NodeId, timestamp_ms: u64) -> Vec<u8> {
        let mut encoder = Canonical::new("memory-tombstone");
        encoder
            .fixed32(memory.as_bytes())
            .fixed32(author.as_bytes())
            .u64(timestamp_ms);
        encoder.finish()
    }

    pub fn sign(identity: &NodeIdentity, memory: Hash32, timestamp_ms: u64) -> Self {
        let author = identity.node_id();
        Self {
            memory,
            author,
            timestamp_ms,
            signature: identity.sign(&Self::message(&memory, &author, timestamp_ms)),
        }
    }

    pub fn verify(&self) -> Result<()> {
        self.author.verify(
            &Self::message(&self.memory, &self.author, self.timestamp_ms),
            &self.signature,
        )
    }
}
