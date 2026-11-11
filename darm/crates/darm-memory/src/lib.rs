//! DARM-ANN distributed associative memory.
//!
//! Memories are signed, content-addressed records. The id is a hash of the
//! canonical record (author included), so any node can check that a record it
//! received over gossip is exactly what its author wrote. Memories carry a
//! [`Scope`]: `Local` records never leave the node that wrote them, `Mesh`
//! records are offered to peers and may be committed to the ledger.
//!
//! The store keeps the original proof of concept's short-term / long-term
//! split: new memories start short-term and decay; memories that are recalled
//! repeatedly, marked important, or finalized on the ledger are consolidated
//! into long-term storage with a much slower decay.
//!
//! Recall is not the same as usefulness. [`outcome`] records whether a
//! recalled memory helped or hurt, and that record ranks memories, gates
//! consolidation and demotes the ones that keep hurting. [`feed`] is a
//! bounded stream of what the store is doing, for live views.

pub mod context;
pub mod embed;
pub mod feed;
pub mod outcome;
pub mod record;
pub mod store;

pub use context::render_context;
pub use embed::{Embedder, HashingEmbedder};
pub use feed::{Feed, FeedEvent, FeedKind};
pub use outcome::{Outcome, Usefulness};
pub use record::{MemoryKind, MemoryRecord, Scope, SignedMemory, SignedTombstone};
pub use store::{
    Hit, InsertOutcome, MemoryLimits, MemoryLog, MemoryStats, MemoryStore, Origin, SearchOptions,
    Tier,
};
