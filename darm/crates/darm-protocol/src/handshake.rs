//! Noise XX handshake bound to DARM node identities.
//!
//! ```text
//!  initiator                                          responder
//!  Init { sid, offer_i, suite, noise: -> e }  ───────►
//!                                              ◄─────  Retry { offer_r }            (suite mismatch)
//!                                              ◄─────  Resp { noise: <- e, ee, s, es,
//!                                                              enc({offer_r, bind_r}) }
//!  Fin { noise: -> s, se, enc({bind_i}) }     ───────►
//! ```
//!
//! - The Noise prologue commits to the protocol, network id, session id,
//!   chosen suite and the initiator's offer, so tampering with any of them
//!   makes the handshake fail.
//! - The responder's offer travels inside the encrypted, authenticated
//!   payload of message 2; the initiator re-runs [`negotiate`] over both
//!   authenticated offers and aborts unless it yields the suite in use. An
//!   unauthenticated `Retry` can therefore only make the initiator restart,
//!   never accept a suite or parameters both sides would not have chosen.
//! - Each side signs, with its long-term Ed25519 identity, the handshake hash
//!   at the point it writes its static key, together with that static key and
//!   its role. The hash covers both ephemerals, so a binding cannot be
//!   replayed into another session, and the role byte prevents reflection.

use crate::session::{Session, SessionLimits};
use crate::suite::{Offer, Selected, Suite, negotiate};
use base64::Engine;
use base64::engine::general_purpose::STANDARD as B64;
use darm_core::canonical::Canonical;
use darm_core::{DarmError, Hash32, NodeId, NodeIdentity, Result, Signature};
use serde::{Deserialize, Serialize};
use zeroize::Zeroizing;

/// Upper bound on any handshake message (base64 noise payloads included).
pub const MAX_HANDSHAKE_BYTES: usize = 8 * 1024;
const NOISE_BUF: usize = 4 * 1024;

const ROLE_INITIATOR: u8 = 1;
const ROLE_RESPONDER: u8 = 2;

fn noise_err(error: snow::Error) -> DarmError {
    DarmError::Rejected(format!("noise: {error}"))
}

/// Process-lifetime X25519 static key for Noise. Authenticated on every
/// handshake by the node's Ed25519 identity, so it is never persisted.
pub struct StaticKey {
    private: Zeroizing<Vec<u8>>,
    public: [u8; 32],
}

impl StaticKey {
    pub fn generate() -> Result<Self> {
        let builder = snow::Builder::new(
            Suite::NoiseXxChaChaPoly
                .noise_params()
                .parse()
                .map_err(noise_err)?,
        );
        let keypair = builder.generate_keypair().map_err(noise_err)?;
        let public: [u8; 32] = keypair
            .public
            .as_slice()
            .try_into()
            .map_err(|_| DarmError::malformed("static key", "expected 32 bytes"))?;
        Ok(Self {
            private: Zeroizing::new(keypair.private),
            public,
        })
    }

    pub fn public(&self) -> &[u8; 32] {
        &self.public
    }
}

impl std::fmt::Debug for StaticKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "StaticKey({})", hex::encode(&self.public[..6]))
    }
}

/// What this node brings to a handshake.
pub struct HandshakeConfig<'a> {
    pub identity: &'a NodeIdentity,
    pub static_key: &'a StaticKey,
    /// Network the session is for (the DARM chain id). Both sides must match.
    pub network: Hash32,
    pub offer: Offer,
    pub limits: SessionLimits,
}

/// Ed25519 proof that `node` owns the Noise static key used in a session.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Binding {
    pub node: NodeId,
    pub signature: Signature,
}

fn binding_message(
    network: &Hash32,
    sid: &[u8; 16],
    role: u8,
    handshake_hash: &[u8],
    static_key: &[u8; 32],
) -> Vec<u8> {
    let mut encoder = Canonical::new("session-bind");
    encoder
        .fixed32(network.as_bytes())
        .bytes(sid)
        .u8(role)
        .bytes(handshake_hash)
        .fixed32(static_key);
    encoder.finish()
}

impl Binding {
    fn sign(config: &HandshakeConfig<'_>, sid: &[u8; 16], role: u8, handshake_hash: &[u8]) -> Self {
        let message = binding_message(
            &config.network,
            sid,
            role,
            handshake_hash,
            config.static_key.public(),
        );
        Self {
            node: config.identity.node_id(),
            signature: config.identity.sign(&message),
        }
    }

    fn verify(
        &self,
        network: &Hash32,
        sid: &[u8; 16],
        role: u8,
        handshake_hash: &[u8],
        remote_static: Option<&[u8]>,
    ) -> Result<()> {
        let remote: [u8; 32] = remote_static
            .and_then(|key| key.try_into().ok())
            .ok_or_else(|| DarmError::Rejected("peer sent no static key".into()))?;
        let message = binding_message(network, sid, role, handshake_hash, &remote);
        self.node.verify(&message, &self.signature)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "step", rename_all = "snake_case")]
pub enum HandshakeMsg {
    Init {
        sid: String,
        offer: Offer,
        suite: Suite,
        noise: String,
    },
    Retry {
        sid: String,
        offer: Offer,
    },
    Resp {
        sid: String,
        noise: String,
    },
    Fin {
        sid: String,
        noise: String,
    },
}

impl HandshakeMsg {
    pub fn sid(&self) -> &str {
        match self {
            Self::Init { sid, .. }
            | Self::Retry { sid, .. }
            | Self::Resp { sid, .. }
            | Self::Fin { sid, .. } => sid,
        }
    }
}

#[derive(Serialize, Deserialize)]
struct RespPayload {
    offer: Offer,
    bind: Binding,
}

#[derive(Serialize, Deserialize)]
struct FinPayload {
    bind: Binding,
}

pub fn parse_sid(value: &str) -> Result<[u8; 16]> {
    if value.len() != 32 {
        return Err(DarmError::malformed("session id", "expected 32 hex chars"));
    }
    let mut sid = [0u8; 16];
    hex::decode_to_slice(value, &mut sid)
        .map_err(|e| DarmError::malformed("session id", e.to_string()))?;
    Ok(sid)
}

fn decode_noise(value: &str) -> Result<Vec<u8>> {
    if value.len() > MAX_HANDSHAKE_BYTES {
        return Err(DarmError::LimitExceeded("handshake message".into()));
    }
    B64.decode(value)
        .map_err(|e| DarmError::malformed("handshake", e.to_string()))
}

fn prologue(network: &Hash32, sid: &[u8; 16], suite: Suite, offer: &Offer) -> Vec<u8> {
    let mut encoder = Canonical::new("session-prologue");
    encoder
        .fixed32(network.as_bytes())
        .bytes(sid)
        .str(suite.noise_params());
    offer.encode(&mut encoder);
    encoder.finish()
}

fn build(
    config: &HandshakeConfig<'_>,
    sid: &[u8; 16],
    suite: Suite,
    offer_i: &Offer,
    initiator: bool,
) -> Result<snow::HandshakeState> {
    let prologue = prologue(&config.network, sid, suite, offer_i);
    let builder = snow::Builder::new(suite.noise_params().parse().map_err(noise_err)?)
        .local_private_key(&config.static_key.private)
        .map_err(noise_err)?
        .prologue(&prologue)
        .map_err(noise_err)?;
    if initiator {
        builder.build_initiator()
    } else {
        builder.build_responder()
    }
    .map_err(noise_err)
}

/// Initiator side, waiting for `Resp` (or one `Retry`).
pub struct Initiator {
    sid: [u8; 16],
    offer: Offer,
    suite: Suite,
    state: snow::HandshakeState,
    retried: bool,
}

impl Initiator {
    /// Start a handshake with the initiator's most preferred suite.
    pub fn start(config: &HandshakeConfig<'_>) -> Result<(Self, HandshakeMsg)> {
        config.offer.validate()?;
        let mut sid = [0u8; 16];
        getrandom::fill(&mut sid).map_err(|e| DarmError::Entropy(e.to_string()))?;
        Self::begin(config, sid, config.offer.suites[0], false)
    }

    fn begin(
        config: &HandshakeConfig<'_>,
        sid: [u8; 16],
        suite: Suite,
        retried: bool,
    ) -> Result<(Self, HandshakeMsg)> {
        let mut state = build(config, &sid, suite, &config.offer, true)?;
        let mut buf = [0u8; NOISE_BUF];
        let len = state.write_message(&[], &mut buf).map_err(noise_err)?;
        let message = HandshakeMsg::Init {
            sid: hex::encode(sid),
            offer: config.offer.clone(),
            suite,
            noise: B64.encode(&buf[..len]),
        };
        Ok((
            Self {
                sid,
                offer: config.offer.clone(),
                suite,
                state,
                retried,
            },
            message,
        ))
    }

    pub fn sid_hex(&self) -> String {
        hex::encode(self.sid)
    }

    /// The responder asked for a different suite. Allowed once; the new
    /// suite must be what negotiation over both offers selects, and it is
    /// re-checked against the authenticated offer in `Resp`.
    pub fn on_retry(
        self,
        config: &HandshakeConfig<'_>,
        offer_r: &Offer,
    ) -> Result<(Self, HandshakeMsg)> {
        if self.retried {
            return Err(DarmError::Rejected("second handshake retry".into()));
        }
        let selected = negotiate(&self.offer, offer_r)?;
        if selected.suite == self.suite {
            return Err(DarmError::Rejected("retry without a suite change".into()));
        }
        Self::begin(config, self.sid, selected.suite, true)
    }

    /// Process `Resp`; returns the session and the `Fin` message to send.
    pub fn on_resp(
        mut self,
        config: &HandshakeConfig<'_>,
        noise: &str,
        now_ms: u64,
    ) -> Result<(Session, HandshakeMsg)> {
        let message = decode_noise(noise)?;
        // The responder signed the hash as it stood before its message 2.
        let responder_hash = self.state.get_handshake_hash().to_vec();
        let mut payload = [0u8; NOISE_BUF];
        let len = self
            .state
            .read_message(&message, &mut payload)
            .map_err(noise_err)?;
        let resp: RespPayload = serde_json::from_slice(&payload[..len])
            .map_err(|e| DarmError::malformed("handshake payload", e.to_string()))?;
        resp.bind.verify(
            &config.network,
            &self.sid,
            ROLE_RESPONDER,
            &responder_hash,
            self.state.get_remote_static(),
        )?;
        if resp.bind.node == config.identity.node_id() {
            return Err(DarmError::Rejected("handshake with ourselves".into()));
        }
        let selected = negotiate(&self.offer, &resp.offer)?;
        if selected.suite != self.suite {
            return Err(DarmError::Rejected(
                "authenticated offers select another suite (downgrade attempt?)".into(),
            ));
        }
        let bind = Binding::sign(
            config,
            &self.sid,
            ROLE_INITIATOR,
            self.state.get_handshake_hash(),
        );
        let fin = serde_json::to_vec(&FinPayload { bind })
            .map_err(|e| DarmError::malformed("handshake payload", e.to_string()))?;
        let mut buf = [0u8; NOISE_BUF];
        let len = self
            .state
            .write_message(&fin, &mut buf)
            .map_err(noise_err)?;
        let session = Session::establish(
            self.state,
            self.sid,
            resp.bind.node,
            selected,
            true,
            config.limits,
            now_ms,
        )?;
        Ok((
            session,
            HandshakeMsg::Fin {
                sid: hex::encode(self.sid),
                noise: B64.encode(&buf[..len]),
            },
        ))
    }
}

/// Responder waiting for `Fin`.
pub struct PendingResponder {
    sid: [u8; 16],
    peer: NodeId,
    selected: Selected,
    state: snow::HandshakeState,
}

pub enum ResponderStep {
    Retry(HandshakeMsg),
    Continue(Box<PendingResponder>, HandshakeMsg),
}

/// Handle an `Init`.
pub fn respond(
    config: &HandshakeConfig<'_>,
    sid: &str,
    offer_i: &Offer,
    suite: Suite,
    noise: &str,
) -> Result<ResponderStep> {
    let sid = parse_sid(sid)?;
    let selected = negotiate(offer_i, &config.offer)?;
    if selected.suite != suite {
        return Ok(ResponderStep::Retry(HandshakeMsg::Retry {
            sid: hex::encode(sid),
            offer: config.offer.clone(),
        }));
    }
    let message = decode_noise(noise)?;
    let mut state = build(config, &sid, suite, offer_i, false)?;
    let mut scratch = [0u8; NOISE_BUF];
    state
        .read_message(&message, &mut scratch)
        .map_err(noise_err)?;
    let bind = Binding::sign(config, &sid, ROLE_RESPONDER, state.get_handshake_hash());
    let payload = serde_json::to_vec(&RespPayload {
        offer: config.offer.clone(),
        bind,
    })
    .map_err(|e| DarmError::malformed("handshake payload", e.to_string()))?;
    let mut buf = [0u8; NOISE_BUF];
    let len = state.write_message(&payload, &mut buf).map_err(noise_err)?;
    let pending = PendingResponder {
        sid,
        // Unknown until `Fin`; replaced there.
        peer: config.identity.node_id(),
        selected,
        state,
    };
    Ok(ResponderStep::Continue(
        Box::new(pending),
        HandshakeMsg::Resp {
            sid: hex::encode(sid),
            noise: B64.encode(&buf[..len]),
        },
    ))
}

impl PendingResponder {
    pub fn on_fin(
        mut self,
        config: &HandshakeConfig<'_>,
        noise: &str,
        now_ms: u64,
    ) -> Result<Session> {
        let message = decode_noise(noise)?;
        let initiator_hash = self.state.get_handshake_hash().to_vec();
        let mut payload = [0u8; NOISE_BUF];
        let len = self
            .state
            .read_message(&message, &mut payload)
            .map_err(noise_err)?;
        let fin: FinPayload = serde_json::from_slice(&payload[..len])
            .map_err(|e| DarmError::malformed("handshake payload", e.to_string()))?;
        fin.bind.verify(
            &config.network,
            &self.sid,
            ROLE_INITIATOR,
            &initiator_hash,
            self.state.get_remote_static(),
        )?;
        if fin.bind.node == config.identity.node_id() {
            return Err(DarmError::Rejected("handshake with ourselves".into()));
        }
        self.peer = fin.bind.node;
        Session::establish(
            self.state,
            self.sid,
            self.peer,
            self.selected,
            false,
            config.limits,
            now_ms,
        )
    }
}

#[cfg(test)]
mod tests;
