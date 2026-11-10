//! DARM-ANN polymorphic and adaptive protocols.
//!
//! mesh-llm carries DARM gossip as plugin channel frames whose claimed source
//! peer is not authenticated, and every DARM record therefore carries its own
//! signature. This crate adds an optional **session layer** on top that:
//!
//! - **negotiates** a cipher suite, payload codec and padding policy per peer
//!   pair ([`suite`]). The choice is *polymorphic*: a phone without AES
//!   hardware, on a metered link, ends up with ChaCha20-Poly1305, CBOR and
//!   light padding with a server, while two servers pick AES-GCM, JSON and
//!   power-of-two padding. Both offers are authenticated by the handshake, so
//!   a man in the middle cannot force a different choice;
//! - runs a **Noise XX** handshake (the `snow` implementation, not a home-grown
//!   key exchange) and binds the resulting channel to each side's DARM node
//!   identity with an Ed25519 signature over the handshake hash
//!   ([`handshake`]). That binding is what lets a node attribute gossip to a
//!   *proven* DARM identity instead of a claimed peer id;
//! - seals messages into **morphing frames** ([`session`]): the explicit
//!   nonce is masked (in the style of QUIC header protection) and plaintexts
//!   are padded to buckets, so no fixed bytes or exact lengths are visible on
//!   the wire. Frames carry a replay window ([`replay`]) and large messages
//!   are fragmented within Noise's 64 KiB limit with bounded reassembly;
//! - **adapts** batch sizes, gossip cadence, fan-out, timeouts and the local
//!   offer to measured link quality and device state ([`adaptive`]): AIMD on
//!   delivery and timeout, RFC 6298 retransmission timers, and conservative
//!   behaviour on battery, metered or hot devices.
//!
//! Static Noise keys are generated per process and authenticated by the
//! node's long-term Ed25519 identity on every handshake, so no additional
//! secret is ever written to disk.

pub mod adaptive;
pub mod handshake;
pub mod replay;
pub mod session;
pub mod suite;

pub use adaptive::{AdaptiveController, DeviceHints, LinkEstimator, Tuning};
pub use handshake::{
    Binding, HandshakeConfig, HandshakeMsg, Initiator, PendingResponder, ResponderStep, StaticKey,
    respond,
};
pub use replay::ReplayWindow;
pub use session::{Session, SessionLimits};
pub use suite::{Codec, Offer, Padding, Selected, Suite, negotiate};

/// Largest decoded message the session layer reassembles. Matches the
/// `darm.v1` channel body limit.
pub const MAX_MESSAGE_BYTES: usize = 1024 * 1024;
