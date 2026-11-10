//! The DARM-ANN distributed memory ledger.
//!
//! Nodes sign [`SignedEntry`] records (memory commits, revocations,
//! reputation reports, proposals, votes, registrations, pooled-compute
//! commits). A designated
//! proposer batches pending entries into a [`Block`]; the block becomes final
//! once validators holding more than two thirds of the voting weight attest
//! to it. Each validator attests to at most one block per height, so with
//! fewer than one third of the weight faulty two conflicting blocks can never
//! both finalize.
//!
//! Liveness: the proposer for `(height, round)` rotates, so an offline
//! proposer is skipped by moving to the next round. A Byzantine proposer that
//! equivocates can stall a height (votes split); full lock-based BFT
//! (Tendermint/HotStuff style) is on the roadmap in `darm/docs/ROADMAP.md`.

mod chain;
mod entry;
mod state;
pub mod store;
mod validators;

pub use chain::{Ledger, LedgerLimits};
pub use entry::{
    Attestation, Block, BlockHeader, EntryBody, LedgerEntry, MAX_COMPUTE_SHARDS, SignedEntry,
    attestation_message, block_proposal_message,
};
pub use state::{ComputeRecord, LedgerState, ProposalOutcome, ProposalState, Registration};
pub use validators::{ChainConfig, SecurityPhase, ValidatorSet};
