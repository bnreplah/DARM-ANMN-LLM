//! DARM-ANN core primitives.
//!
//! DARM-ANN (Distributed Agentic Resilient Memory over Artificial Neural
//! Networks) gives a mesh of LLM nodes a shared, tamper-evident memory. This
//! crate holds the pieces every implementation must agree on byte-for-byte:
//!
//! - [`identity`]: Ed25519 node identities. A node id *is* its public key, so
//!   every record can be verified without a registry lookup.
//! - [`canonical`]: the length-prefixed, domain-separated encoding that is
//!   signed and hashed. JSON is only a transport; it is never signed.
//! - [`hash`] and [`merkle`]: domain-separated BLAKE3 content ids and Merkle
//!   trees with inclusion proofs (used for light clients and on-chain anchors).
//! - [`ledger`]: signed entries batched into blocks that become final once
//!   validators holding more than two thirds of the voting weight attest.
//!
//! The original DARM-ANN proof of concept simulated its cryptography (an XOR
//! cipher and an HMAC "signature" that only the signer could verify) and used
//! proof-of-work for blocks. Both are replaced here: signatures are real
//! public-key signatures and finality comes from validator attestations, which
//! gives Sybil resistance in an admitted mesh without burning compute.

pub mod canonical;
pub mod error;
pub mod hash;
pub mod identity;
pub mod ledger;
pub mod merkle;
pub mod time;

pub use error::{DarmError, Result};
pub use hash::Hash32;
pub use identity::{NodeId, NodeIdentity, Signature};

/// Protocol version tag mixed into every signing and hashing domain.
pub const PROTOCOL: &str = "DARM-ANN/v1";
