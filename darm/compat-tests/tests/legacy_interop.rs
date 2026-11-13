//! Wire compatibility with nodes built before the hybrid post-quantum suites
//! (DARM-ANMN-LLM 3769ea6). Messages cross between the two implementations
//! as JSON, exactly as on the wire.

use darm_core::{Hash32, NodeIdentity};
use darm_protocol::{
    HandshakeConfig, HandshakeMsg, Initiator, Offer, PostQuantum, ResponderStep, Session,
    SessionLimits, StaticKey, Suite, respond,
};
use legacy_protocol as old;
use serde::Serialize;
use serde::de::DeserializeOwned;

const NOW: u64 = 1_700_000_000_000;

/// Re-encode a message as the other implementation would receive it;
/// `None` if the receiver cannot parse it (the handshake fails there).
fn wire<T: Serialize, U: DeserializeOwned>(message: &T) -> Option<U> {
    let bytes = serde_json::to_vec(message).unwrap();
    serde_json::from_slice(&bytes).ok()
}

struct Current {
    identity: NodeIdentity,
    key: StaticKey,
    offer: Offer,
}

impl Current {
    fn new(seed: u8, policy: PostQuantum) -> Self {
        Self {
            identity: NodeIdentity::from_secret_bytes(&[seed; 32]),
            key: StaticKey::generate().unwrap(),
            offer: Offer::with_post_quantum(policy),
        }
    }
    fn config(&self) -> HandshakeConfig<'_> {
        HandshakeConfig {
            identity: &self.identity,
            static_key: &self.key,
            network: Hash32::digest("net", b"interop"),
            offer: self.offer.clone(),
            limits: SessionLimits::default(),
        }
    }
}

struct Legacy {
    identity: legacy_core::NodeIdentity,
    key: old::StaticKey,
}

impl Legacy {
    fn new(seed: u8) -> Self {
        Self {
            identity: legacy_core::NodeIdentity::from_secret_bytes(&[seed; 32]),
            key: old::StaticKey::generate().unwrap(),
        }
    }
    fn config(&self) -> old::HandshakeConfig<'_> {
        old::HandshakeConfig {
            identity: &self.identity,
            static_key: &self.key,
            network: legacy_core::Hash32::digest("net", b"interop"),
            offer: old::Offer::default(),
            limits: old::SessionLimits::default(),
        }
    }
}

/// Current initiator, old responder. Returns the suite and both sessions.
fn new_to_old(
    a: &Current,
    b: &Legacy,
    peer_hybrid: bool,
) -> Option<(Suite, Session, old::Session)> {
    let (mut initiator, mut init) = Initiator::start_for(&a.config(), peer_hybrid).ok()?;
    for _ in 0..2 {
        let old::HandshakeMsg::Init {
            sid,
            offer,
            suite,
            noise,
        } = wire::<_, old::HandshakeMsg>(&init)?
        else {
            unreachable!()
        };
        match old::respond(&b.config(), &sid, &offer, suite, &noise).ok()? {
            old::ResponderStep::Retry(retry) => {
                let HandshakeMsg::Retry { offer, .. } = wire(&retry)? else {
                    unreachable!()
                };
                let (next, message) = initiator.on_retry(&a.config(), &offer).ok()?;
                initiator = next;
                init = message;
            }
            old::ResponderStep::Continue(pending, resp) => {
                let HandshakeMsg::Resp { noise, .. } = wire(&resp)? else {
                    unreachable!()
                };
                let (session_a, fin) = initiator.on_resp(&a.config(), &noise, NOW).ok()?;
                let old::HandshakeMsg::Fin { noise, .. } = wire(&fin)? else {
                    unreachable!()
                };
                let session_b = pending.on_fin(&b.config(), &noise, NOW).ok()?;
                return Some((session_a.selected().suite, session_a, session_b));
            }
        }
    }
    None
}

/// Legacy initiator, new responder.
fn old_to_new(a: &Legacy, b: &Current) -> Option<(Suite, old::Session, Session)> {
    let (mut initiator, mut init) = old::Initiator::start(&a.config()).ok()?;
    for _ in 0..2 {
        let HandshakeMsg::Init {
            sid,
            offer,
            suite,
            noise,
        } = wire::<_, HandshakeMsg>(&init)?
        else {
            unreachable!()
        };
        match respond(&b.config(), &sid, &offer, suite, &noise).ok()? {
            ResponderStep::Retry(retry) => {
                let old::HandshakeMsg::Retry { offer, .. } = wire(&retry)? else {
                    unreachable!()
                };
                let (next, message) = initiator.on_retry(&a.config(), &offer).ok()?;
                initiator = next;
                init = message;
            }
            ResponderStep::Continue(pending, resp) => {
                let old::HandshakeMsg::Resp { noise, .. } = wire(&resp)? else {
                    unreachable!()
                };
                let (session_a, fin) = initiator.on_resp(&a.config(), &noise, NOW).ok()?;
                let HandshakeMsg::Fin { noise, .. } = wire(&fin)? else {
                    unreachable!()
                };
                let session_b = pending.on_fin(&b.config(), &noise, NOW).ok()?;
                return Some((session_b.selected().suite, session_a, session_b));
            }
        }
    }
    None
}

fn exchange(new: &mut Session, old: &mut old::Session) {
    for frame in new.seal(b"from new", NOW).unwrap() {
        if let Some(message) = old.open(&frame, NOW).unwrap() {
            assert_eq!(message, b"from new");
        }
    }
    for frame in old.seal(b"from old", NOW).unwrap() {
        if let Some(message) = new.open(&frame, NOW).unwrap() {
            assert_eq!(message, b"from old");
        }
    }
}

#[test]
fn post_quantum_preferred_nodes_talk_classically_to_older_nodes() {
    let new = Current::new(1, PostQuantum::Preferred);
    let older = Legacy::new(2);

    let (suite, mut a, mut b) = new_to_old(&new, &older, false).expect("new -> old");
    assert!(!suite.hybrid());
    exchange(&mut a, &mut b);

    let (suite, mut a, mut b) = old_to_new(&older, &new).expect("old -> new");
    assert!(!suite.hybrid());
    exchange(&mut b, &mut a);
}

#[test]
fn post_quantum_off_is_unchanged_on_the_wire() {
    let new = Current::new(1, PostQuantum::Off);
    let older = Legacy::new(2);
    assert!(new_to_old(&new, &older, false).is_some());
    assert!(old_to_new(&older, &new).is_some());
    let json = serde_json::to_value(&new.offer).unwrap();
    assert!(json.get("suites_ext").is_none(), "{json}");
}

#[test]
fn post_quantum_required_refuses_older_nodes() {
    let new = Current::new(1, PostQuantum::Required);
    let older = Legacy::new(2);
    assert!(new_to_old(&new, &older, false).is_none());
    assert!(old_to_new(&older, &new).is_none());
}

#[test]
fn a_wrong_hybrid_hint_still_fails_safe() {
    // Opening with a hybrid suite towards an old node fails (it cannot parse
    // the suite); callers then retry without the hint.
    let new = Current::new(1, PostQuantum::Preferred);
    let older = Legacy::new(2);
    let (_, init) = Initiator::start_for(&new.config(), true).unwrap();
    let bytes = serde_json::to_vec(&init).unwrap();
    assert!(serde_json::from_slice::<old::HandshakeMsg>(&bytes).is_err());
    assert!(new_to_old(&new, &older, false).is_some());
}
