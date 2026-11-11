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
//! proposer is skipped by moving to the next round. Under the one-vote rule
//! alone a Byzantine proposer that equivocates can stall a height (votes
//! split). [`locking`] is the lock-based replacement for that rule: prevote
//! and precommit rounds in which validators may vote again in a later round
//! without ever being able to finalize two blocks. A ledger switches to it
//! with [`Ledger::enable_locking`]; blocks it finalizes carry their
//! `commit_round` and that round's precommits as the commit certificate.

mod chain;
mod entry;
pub mod locking;
mod state;
pub mod store;
mod validators;

pub use chain::{Ledger, LedgerLimits, RoundProposal};
pub use entry::{
    Attestation, Block, BlockHeader, CREDIT_GRANT_KIND, EntryBody, LedgerEntry, MAX_COMPUTE_SHARDS,
    MAX_CREDIT_AMOUNT, MAX_SETTLE_PAYEES, Payment, SignedEntry, attestation_message,
    block_proposal_message, credit_grant_payload,
};
pub use state::{ComputeRecord, LedgerState, ProposalOutcome, ProposalState, Registration};
pub use validators::{ChainConfig, SecurityPhase, ValidatorSet};
