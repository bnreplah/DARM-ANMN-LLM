//! Per-peer Noise sessions and adaptive link state for the `darm.v1`
//! channel (see `darm_protocol` for the protocol itself).
//!
//! Every handshake costs two X25519 operations and an Ed25519 signature, so
//! a peer may start at most one handshake per [`INIT_COOLDOWN_MS`], pending
//! handshakes are capped and time out, and the session table is bounded.

use crate::config::SessionMode;
use crate::wire::{Outbound, Wire};
use base64::Engine;
use base64::engine::general_purpose::STANDARD as B64;
use darm_core::{DarmError, Hash32, NodeId, NodeIdentity, Result};
use darm_protocol::adaptive::{hardware_aes, local_offer};
use darm_protocol::{
    AdaptiveController, DeviceHints, HandshakeConfig, HandshakeMsg, Initiator, PendingResponder,
    ResponderStep, Session, SessionLimits, StaticKey, Tuning, respond,
};
use serde_json::{Value, json};
use std::collections::{BTreeSet, HashMap};

const HANDSHAKE_TIMEOUT_MS: u64 = 20_000;
pub const INIT_COOLDOWN_MS: u64 = 5_000;
const RETRY_INTERVAL_MS: u64 = 60_000;
const MAX_PENDING: usize = 256;
const MAX_SESSIONS: usize = 4_096;
const MAX_KNOWN_PEERS: usize = 4_096;

struct PendingInit {
    initiator: Initiator,
    started_ms: u64,
}

struct PendingResp {
    responder: Box<PendingResponder>,
    sid: String,
    started_ms: u64,
}

pub struct Sessions {
    pub mode: SessionMode,
    static_key: StaticKey,
    aes: bool,
    pub hints: DeviceHints,
    sessions: HashMap<String, Session>,
    initiating: HashMap<String, PendingInit>,
    responding: HashMap<String, PendingResp>,
    links: HashMap<String, AdaptiveController>,
    known_peers: BTreeSet<String>,
    last_init_from: HashMap<String, u64>,
    last_attempt: HashMap<String, u64>,
    pub handshakes_completed: u64,
    pub handshakes_failed: u64,
}

impl Sessions {
    pub fn new(mode: SessionMode, hints: DeviceHints) -> Result<Self> {
        Ok(Self {
            mode,
            static_key: StaticKey::generate()?,
            aes: hardware_aes(),
            hints,
            sessions: HashMap::new(),
            initiating: HashMap::new(),
            responding: HashMap::new(),
            links: HashMap::new(),
            known_peers: BTreeSet::new(),
            last_init_from: HashMap::new(),
            last_attempt: HashMap::new(),
            handshakes_completed: 0,
            handshakes_failed: 0,
        })
    }

    fn config<'a>(&'a self, identity: &'a NodeIdentity, network: Hash32) -> HandshakeConfig<'a> {
        HandshakeConfig {
            identity,
            static_key: &self.static_key,
            network,
            offer: local_offer(&self.hints, self.aes),
            limits: SessionLimits::default(),
        }
    }

    pub fn enabled(&self) -> bool {
        self.mode != SessionMode::Off
    }

    pub fn session_peer(&self, peer: &str) -> Option<NodeId> {
        self.sessions.get(peer).map(Session::peer)
    }

    /// Smoothed RTT to the mesh peer whose session proved `node`.
    pub fn rtt_to(&self, node: &NodeId) -> Option<f64> {
        self.sessions
            .iter()
            .find(|(_, s)| s.peer() == *node)
            .and_then(|(peer, _)| self.links.get(peer))
            .and_then(|link| link.link.srtt_ms)
    }

    pub fn tuning(&self, peer: &str) -> Tuning {
        self.links.get(peer).map_or_else(
            || AdaptiveController::default().tuning(&self.hints),
            |c| c.tuning(&self.hints),
        )
    }

    /// Tuning for this node as a whole (no particular peer).
    pub fn local_tuning(&self) -> Tuning {
        AdaptiveController::default().tuning(&self.hints)
    }

    pub fn note_peer(&mut self, peer: &str) {
        if self.known_peers.len() < MAX_KNOWN_PEERS || self.known_peers.contains(peer) {
            self.known_peers.insert(peer.to_string());
        }
    }

    pub fn link(&mut self, peer: &str) -> &mut AdaptiveController {
        if self.links.len() >= MAX_KNOWN_PEERS && !self.links.contains_key(peer) {
            self.links.clear();
        }
        self.links.entry(peer.to_string()).or_default()
    }

    /// Start a handshake with `peer` unless one exists or is in flight.
    pub fn initiate(
        &mut self,
        peer: &str,
        identity: &NodeIdentity,
        network: Hash32,
        now_ms: u64,
    ) -> Vec<Outbound> {
        if !self.enabled()
            || peer.is_empty()
            || self.sessions.contains_key(peer)
            || self.initiating.contains_key(peer)
            || self.initiating.len() >= MAX_PENDING
        {
            return Vec::new();
        }
        self.note_peer(peer);
        self.last_attempt.insert(peer.to_string(), now_ms);
        match Initiator::start(&self.config(identity, network)) {
            Ok((initiator, message)) => {
                self.initiating.insert(
                    peer.to_string(),
                    PendingInit {
                        initiator,
                        started_ms: now_ms,
                    },
                );
                vec![Outbound::to(peer, Wire::Handshake { handshake: message })]
            }
            Err(error) => {
                tracing::debug!(%error, "darm handshake start failed");
                Vec::new()
            }
        }
    }

    /// Handle a handshake message from `peer`.
    pub fn on_handshake(
        &mut self,
        peer: &str,
        message: HandshakeMsg,
        identity: &NodeIdentity,
        network: Hash32,
        now_ms: u64,
    ) -> Result<Vec<Outbound>> {
        if !self.enabled() {
            return Ok(Vec::new());
        }
        let result = self.on_handshake_inner(peer, message, identity, network, now_ms);
        if result.is_err() {
            self.handshakes_failed += 1;
        }
        result
    }

    fn on_handshake_inner(
        &mut self,
        peer: &str,
        message: HandshakeMsg,
        identity: &NodeIdentity,
        network: Hash32,
        now_ms: u64,
    ) -> Result<Vec<Outbound>> {
        match message {
            HandshakeMsg::Init {
                sid,
                offer,
                suite,
                noise,
            } => {
                if let Some(last) = self.last_init_from.get(peer)
                    && now_ms.saturating_sub(*last) < INIT_COOLDOWN_MS
                {
                    return Err(DarmError::LimitExceeded("handshake cooldown".into()));
                }
                // Simultaneous open: the lower session id wins.
                if let Some(mine) = self.initiating.get(peer) {
                    if mine.initiator.sid_hex() < sid {
                        return Ok(Vec::new());
                    }
                    self.initiating.remove(peer);
                }
                if self.responding.len() >= MAX_PENDING && !self.responding.contains_key(peer) {
                    return Err(DarmError::LimitExceeded("pending handshakes".into()));
                }
                if self.last_init_from.len() >= MAX_KNOWN_PEERS {
                    self.last_init_from
                        .retain(|_, t| now_ms.saturating_sub(*t) < INIT_COOLDOWN_MS);
                }
                self.last_init_from.insert(peer.to_string(), now_ms);
                self.note_peer(peer);
                match respond(&self.config(identity, network), &sid, &offer, suite, &noise)? {
                    ResponderStep::Retry(retry) => {
                        // Allow the immediate retried Init through the cooldown.
                        self.last_init_from.remove(peer);
                        Ok(vec![Outbound::to(
                            peer,
                            Wire::Handshake { handshake: retry },
                        )])
                    }
                    ResponderStep::Continue(responder, resp) => {
                        self.responding.insert(
                            peer.to_string(),
                            PendingResp {
                                responder,
                                sid,
                                started_ms: now_ms,
                            },
                        );
                        Ok(vec![Outbound::to(
                            peer,
                            Wire::Handshake { handshake: resp },
                        )])
                    }
                }
            }
            HandshakeMsg::Retry { sid, offer } => {
                let Some(pending) = self.initiating.remove(peer) else {
                    return Ok(Vec::new());
                };
                if pending.initiator.sid_hex() != sid {
                    self.initiating.insert(peer.to_string(), pending);
                    return Ok(Vec::new());
                }
                let (initiator, message) = pending
                    .initiator
                    .on_retry(&self.config(identity, network), &offer)?;
                self.initiating.insert(
                    peer.to_string(),
                    PendingInit {
                        initiator,
                        started_ms: pending.started_ms,
                    },
                );
                Ok(vec![Outbound::to(
                    peer,
                    Wire::Handshake { handshake: message },
                )])
            }
            HandshakeMsg::Resp { sid, noise } => {
                let Some(pending) = self.initiating.remove(peer) else {
                    return Ok(Vec::new());
                };
                if pending.initiator.sid_hex() != sid {
                    self.initiating.insert(peer.to_string(), pending);
                    return Ok(Vec::new());
                }
                let started = pending.started_ms;
                let (session, fin) =
                    pending
                        .initiator
                        .on_resp(&self.config(identity, network), &noise, now_ms)?;
                self.link(peer)
                    .on_delivery(now_ms.saturating_sub(started) as f64);
                self.install(peer, session);
                Ok(vec![Outbound::to(peer, Wire::Handshake { handshake: fin })])
            }
            HandshakeMsg::Fin { sid, noise } => {
                let Some(pending) = self.responding.remove(peer) else {
                    return Ok(Vec::new());
                };
                if pending.sid != sid {
                    self.responding.insert(peer.to_string(), pending);
                    return Ok(Vec::new());
                }
                let session =
                    pending
                        .responder
                        .on_fin(&self.config(identity, network), &noise, now_ms)?;
                self.link(peer)
                    .on_delivery(now_ms.saturating_sub(pending.started_ms) as f64);
                self.install(peer, session);
                Ok(Vec::new())
            }
        }
    }

    fn install(&mut self, peer: &str, session: Session) {
        if self.sessions.len() >= MAX_SESSIONS && !self.sessions.contains_key(peer) {
            tracing::debug!("darm session table full");
            return;
        }
        tracing::debug!(peer, darm_node = %session.peer(), selected = ?session.selected(), "darm session established");
        self.handshakes_completed += 1;
        self.sessions.insert(peer.to_string(), session);
    }

    /// Decrypt a sealed frame from `peer`. Returns the inner message and the
    /// proven DARM node once a whole message has arrived.
    pub fn open(&mut self, peer: &str, frame: &str, now_ms: u64) -> Result<Option<(Wire, NodeId)>> {
        let Some(session) = self.sessions.get_mut(peer) else {
            return Err(DarmError::Rejected("sealed frame without a session".into()));
        };
        let bytes = B64
            .decode(frame)
            .map_err(|e| DarmError::malformed("sealed frame", e.to_string()))?;
        let Some(message) = session.open(&bytes, now_ms)? else {
            return Ok(None);
        };
        let inner: Wire = session.decode(&message)?;
        if inner.is_session_control() {
            return Err(DarmError::Rejected("nested session control".into()));
        }
        Ok(Some((inner, session.peer())))
    }

    fn seal(&mut self, peer: &str, message: &Wire, now_ms: u64) -> Result<Vec<Outbound>> {
        let session = self
            .sessions
            .get_mut(peer)
            .ok_or_else(|| DarmError::Rejected("no session".into()))?;
        let encoded = session.encode(message)?;
        Ok(session
            .seal(&encoded, now_ms)?
            .into_iter()
            .map(|frame| {
                Outbound::to(
                    peer,
                    Wire::Sealed {
                        frame: B64.encode(frame),
                    },
                )
            })
            .collect())
    }

    /// Turn plaintext outbound gossip into what actually goes on the wire.
    ///
    /// - handshakes always go as they are;
    /// - targeted messages are sealed when a session exists; without one
    ///   they go plaintext in `Preferred` mode and are dropped in `Required`;
    /// - broadcasts become per-peer sealed copies when every known peer has
    ///   a session (or always in `Required`), and stay a plaintext broadcast
    ///   otherwise.
    pub fn transform(&mut self, outbound: Vec<Outbound>, now_ms: u64) -> Vec<Outbound> {
        if !self.enabled() {
            return outbound;
        }
        let mut out = Vec::with_capacity(outbound.len());
        for item in outbound {
            if item.message.is_session_control() {
                out.push(item);
                continue;
            }
            if !item.target.is_empty() {
                if self.sessions.contains_key(&item.target) {
                    match self.seal(&item.target.clone(), &item.message, now_ms) {
                        Ok(mut frames) => out.append(&mut frames),
                        Err(error) => tracing::debug!(%error, "darm seal failed"),
                    }
                } else if self.mode == SessionMode::Preferred {
                    out.push(item);
                }
                continue;
            }
            let all_sealed = !self.known_peers.is_empty()
                && self
                    .known_peers
                    .iter()
                    .all(|p| self.sessions.contains_key(p));
            if self.mode == SessionMode::Required || all_sealed {
                let peers: Vec<String> = self.sessions.keys().cloned().collect();
                for peer in peers {
                    match self.seal(&peer, &item.message, now_ms) {
                        Ok(mut frames) => out.append(&mut frames),
                        Err(error) => tracing::debug!(%error, "darm seal failed"),
                    }
                }
            } else {
                out.push(item);
            }
        }
        out
    }

    /// Expire stale handshakes and sessions and retry peers without one.
    pub fn tick(&mut self, identity: &NodeIdentity, network: Hash32, now_ms: u64) -> Vec<Outbound> {
        if !self.enabled() {
            return Vec::new();
        }
        let timed_out: Vec<String> = self
            .initiating
            .iter()
            .filter(|(_, p)| now_ms.saturating_sub(p.started_ms) > HANDSHAKE_TIMEOUT_MS)
            .map(|(peer, _)| peer.clone())
            .collect();
        for peer in timed_out {
            self.initiating.remove(&peer);
            self.handshakes_failed += 1;
            self.link(&peer).on_timeout();
        }
        self.responding
            .retain(|_, p| now_ms.saturating_sub(p.started_ms) <= HANDSHAKE_TIMEOUT_MS);
        self.sessions.retain(|_, s| !s.expired(now_ms));
        let retry_after = RETRY_INTERVAL_MS.saturating_mul(self.hints.slowdown());
        let due: Vec<String> = self
            .known_peers
            .iter()
            .filter(|p| !self.sessions.contains_key(*p) && !self.initiating.contains_key(*p))
            .filter(|p| {
                self.last_attempt
                    .get(*p)
                    .is_none_or(|t| now_ms.saturating_sub(*t) >= retry_after)
            })
            .cloned()
            .collect();
        let mut out = Vec::new();
        for peer in due {
            out.extend(self.initiate(&peer, identity, network, now_ms));
        }
        out
    }

    pub fn status(&self) -> Value {
        let sessions: Vec<Value> = self
            .sessions
            .iter()
            .take(64)
            .map(|(peer, s)| {
                let link = self.links.get(peer);
                json!({
                    "peer": peer,
                    "darm_node": s.peer(),
                    "selected": s.selected(),
                    "established_ms": s.established_ms(),
                    "frames_sent": s.frames_sent,
                    "frames_received": s.frames_received,
                    "frames_rejected": s.frames_rejected,
                    "srtt_ms": link.and_then(|l| l.link.srtt_ms),
                    "tuning": self.tuning(peer),
                })
            })
            .collect();
        json!({
            "mode": self.mode,
            "hardware_aes": self.aes,
            "hints": self.hints,
            "local_offer": local_offer(&self.hints, self.aes),
            "established": self.sessions.len(),
            "pending": self.initiating.len() + self.responding.len(),
            "known_peers": self.known_peers.len(),
            "handshakes_completed": self.handshakes_completed,
            "handshakes_failed": self.handshakes_failed,
            "sessions": sessions,
        })
    }
}
