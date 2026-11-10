//! DARM-ANN fabric: the shim other workloads run across.
//!
//! `darm-protocol` gives two nodes an authenticated, negotiated session.
//! This crate turns that session into a **fabric link**: many independent
//! workload channels share one link, each with its own priority class, size
//! limit and rate limit, and the link can ride on any carrier that can move
//! opaque frames.
//!
//! - [`channel`]: channel ids, the four sync classes of the Locality Doctrine
//!   (`async`, `block`, `pipeline`, `tight`) and the per-link channel table.
//! - [`envelope`]: the 12-byte binary header that multiplexes channels and
//!   streams inside a sealed session message.
//! - [`scheduler`]: bounded outbound queues. `tight` traffic always leaves
//!   first and expires instead of arriving late; the other classes share the
//!   link by weight so bulk gossip can neither starve nor be starved.
//! - [`link`]: one peer link. Seals outbound envelopes, opens inbound frames,
//!   and refuses unknown channels, oversized payloads and floods.
//! - [`tensor`]: activation and tensor streams. Typed, shape-checked,
//!   optionally 8-bit quantized, chunked to one frame each so a large tensor
//!   never blocks the link, and verified against a digest on reassembly.
//! - [`transport`]: the carrier abstraction plus an in-process and a TCP
//!   carrier. A mesh-llm channel, QUIC stream or radio link implements the
//!   same two methods.
//! - [`establish`]: runs the `darm-protocol` handshake over a carrier.
//! - [`workload`]: the trait a workload implements to be mounted on a
//!   channel, and the dispatcher that routes inbound messages to it.
//!
//! The link itself performs no I/O and reads no clock: callers pass frames
//! and timestamps in, which keeps it testable and lets it sit under an async
//! runtime, a thread, or a simulator unchanged.
//!
//! Security model: every byte on a link is sealed by the session, so the
//! peer's node id is proven rather than claimed. Everything decoded here is
//! still treated as hostile: lengths are checked before allocation, counts
//! and concurrent reassemblies are bounded, and reserved bits must be zero.

pub mod channel;
pub mod envelope;
pub mod establish;
pub mod link;
pub mod scheduler;
pub mod tensor;
pub mod transport;
pub mod workload;
