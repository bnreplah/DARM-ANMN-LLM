use super::*;
use crate::adaptive::local_offer;
use crate::{Codec, DeviceHints, Padding};

const NOW: u64 = 1_700_000_000_000;

struct Side {
    identity: NodeIdentity,
    key: StaticKey,
    offer: Offer,
}

impl Side {
    fn new(seed: u8, offer: Offer) -> Self {
        Self {
            identity: NodeIdentity::from_secret_bytes(&[seed; 32]),
            key: StaticKey::generate().unwrap(),
            offer,
        }
    }

    fn config(&self) -> HandshakeConfig<'_> {
        HandshakeConfig {
            identity: &self.identity,
            static_key: &self.key,
            network: Hash32::digest("net", b"test"),
            offer: self.offer.clone(),
            limits: SessionLimits::default(),
        }
    }
}

/// Drive a full handshake, honouring at most one retry.
fn handshake(a: &Side, b: &Side) -> Result<(Session, Session)> {
    let (mut initiator, mut init) = Initiator::start(&a.config())?;
    for _ in 0..2 {
        let HandshakeMsg::Init {
            sid,
            offer,
            suite,
            noise,
        } = &init
        else {
            unreachable!()
        };
        match respond(&b.config(), sid, offer, *suite, noise)? {
            ResponderStep::Retry(HandshakeMsg::Retry { offer, .. }) => {
                let (next, message) = initiator.on_retry(&a.config(), &offer)?;
                initiator = next;
                init = message;
            }
            ResponderStep::Retry(_) => unreachable!(),
            ResponderStep::Continue(pending, HandshakeMsg::Resp { noise, .. }) => {
                let (session_a, fin) = initiator.on_resp(&a.config(), &noise, NOW)?;
                let HandshakeMsg::Fin { noise, .. } = fin else {
                    unreachable!()
                };
                let session_b = pending.on_fin(&b.config(), &noise, NOW)?;
                return Ok((session_a, session_b));
            }
            ResponderStep::Continue(..) => unreachable!(),
        }
    }
    Err(DarmError::Rejected("did not converge".into()))
}

#[test]
fn sessions_bind_identities_and_round_trip() {
    let a = Side::new(1, Offer::default());
    let b = Side::new(2, Offer::default());
    let (mut sa, mut sb) = handshake(&a, &b).unwrap();
    assert_eq!(sa.peer(), b.identity.node_id());
    assert_eq!(sb.peer(), a.identity.node_id());
    assert_eq!(sa.selected(), sb.selected());

    let frames = sa.seal(b"hello darm", NOW).unwrap();
    assert_eq!(frames.len(), 1);
    assert_eq!(sb.open(&frames[0], NOW).unwrap().unwrap(), b"hello darm");
    // Replays are rejected; the window only moves on authentic frames.
    assert!(sb.open(&frames[0], NOW).is_err());
    let mut forged = frames[0].clone();
    *forged.last_mut().unwrap() ^= 1;
    assert!(sb.open(&forged, NOW).is_err());
    // And the other direction works with independent keys.
    let back = sb.seal(b"ack", NOW).unwrap();
    assert_eq!(sa.open(&back[0], NOW).unwrap().unwrap(), b"ack");
    assert!(sa.open(&frames[0], NOW).is_err(), "no reflection");
}

#[test]
fn frames_are_padded_and_header_masked() {
    let a = Side::new(1, Offer::default());
    let b = Side::new(2, Offer::default());
    let (mut sa, _) = handshake(&a, &b).unwrap();
    assert_eq!(sa.selected().padding, Padding::PowerOfTwo);
    let short = sa.seal(b"x", NOW).unwrap().remove(0);
    let longer = sa.seal(&[7u8; 100], NOW).unwrap().remove(0);
    assert_eq!(short.len(), longer.len(), "lengths hidden inside a bucket");
    // Consecutive nonces 0 and 1 must not appear in clear.
    assert_ne!(&short[..8], &0u64.to_be_bytes());
    assert_ne!(&longer[..8], &1u64.to_be_bytes());
}

#[test]
fn large_messages_fragment_and_reassemble_out_of_order() {
    let a = Side::new(1, Offer::default());
    let b = Side::new(2, Offer::default());
    let (mut sa, mut sb) = handshake(&a, &b).unwrap();
    let message: Vec<u8> = (0..200_000u32).map(|i| (i % 251) as u8).collect();
    let mut frames = sa.seal(&message, NOW).unwrap();
    assert!(frames.len() > 1);
    frames.reverse();
    let mut out = None;
    for frame in &frames {
        if let Some(done) = sb.open(frame, NOW).unwrap() {
            out = Some(done);
        }
    }
    assert_eq!(out.unwrap(), message);
    assert!(
        sa.seal(&vec![0; crate::MAX_MESSAGE_BYTES + 1], NOW)
            .is_err()
    );
}

#[test]
fn polymorphic_selection_with_retry() {
    // A server preferring AES talks to a ChaCha-only phone: the first Init
    // uses AES, the phone asks for a retry, and the session ends up on
    // ChaCha with the phone's compact codec.
    let server = Side::new(1, local_offer(&DeviceHints::default(), true));
    let phone_offer = Offer {
        suites: vec![Suite::NoiseXxChaChaPoly],
        codecs: vec![Codec::Cbor, Codec::Json],
        paddings: vec![Padding::Minimal, Padding::Block256],
    };
    let phone = Side::new(2, phone_offer);
    let (mut s1, mut s2) = handshake(&server, &phone).unwrap();
    assert_eq!(s1.selected().suite, Suite::NoiseXxChaChaPoly);
    #[derive(Serialize, Deserialize, PartialEq, Debug)]
    struct Probe {
        n: u32,
        s: String,
    }
    let probe = Probe {
        n: 7,
        s: "cbor or json".into(),
    };
    let encoded = s1.encode(&probe).unwrap();
    let frame = s1.seal(&encoded, NOW).unwrap().remove(0);
    let opened = s2.open(&frame, NOW).unwrap().unwrap();
    assert_eq!(s2.decode::<Probe>(&opened).unwrap(), probe);
}

#[test]
fn tampered_offer_or_wrong_network_fails() {
    let a = Side::new(1, Offer::default());
    let b = Side::new(2, Offer::default());
    let (initiator, init) = Initiator::start(&a.config()).unwrap();
    let HandshakeMsg::Init {
        sid, suite, noise, ..
    } = init
    else {
        unreachable!()
    };
    // A MITM strips the initiator's codec list down to CBOR only.
    let tampered = Offer {
        codecs: vec![Codec::Cbor],
        ..Offer::default()
    };
    let ResponderStep::Continue(_, HandshakeMsg::Resp { noise: resp, .. }) =
        respond(&b.config(), &sid, &tampered, suite, &noise).unwrap()
    else {
        panic!("expected a response")
    };
    assert!(
        initiator.on_resp(&a.config(), &resp, NOW).is_err(),
        "prologue mismatch must fail the handshake"
    );

    let (initiator, init) = Initiator::start(&a.config()).unwrap();
    let HandshakeMsg::Init {
        sid,
        offer,
        suite,
        noise,
    } = init
    else {
        unreachable!()
    };
    let mut other = b.config();
    other.network = Hash32::digest("net", b"other");
    let ResponderStep::Continue(_, HandshakeMsg::Resp { noise: resp, .. }) =
        respond(&other, &sid, &offer, suite, &noise).unwrap()
    else {
        panic!("expected a response")
    };
    assert!(initiator.on_resp(&a.config(), &resp, NOW).is_err());
}

#[test]
fn retry_cannot_loop_or_be_forged_into_a_noop() {
    let a = Side::new(1, Offer::default());
    let (initiator, _) = Initiator::start(&a.config()).unwrap();
    // A forged retry naming the same suite is refused.
    assert!(initiator.on_retry(&a.config(), &Offer::default()).is_err());
    let (initiator, _) = Initiator::start(&a.config()).unwrap();
    let aes_first = Offer {
        suites: vec![Suite::NoiseXxAesGcm],
        ..Offer::default()
    };
    let (initiator, _) = initiator.on_retry(&a.config(), &aes_first).unwrap();
    assert!(initiator.on_retry(&a.config(), &Offer::default()).is_err());
}

#[test]
fn self_handshake_is_rejected() {
    let a = Side::new(1, Offer::default());
    let a2 = Side::new(1, Offer::default());
    assert!(handshake(&a, &a2).is_err());
}

#[test]
fn expired_sessions_refuse_to_seal() {
    let a = Side::new(1, Offer::default());
    let b = Side::new(2, Offer::default());
    let (mut sa, _) = handshake(&a, &b).unwrap();
    let later = NOW + SessionLimits::default().max_lifetime_ms;
    assert!(sa.expired(later));
    assert!(sa.seal(b"x", later).is_err());
}

// ------------------------------------------------ hybrid post-quantum

use crate::PostQuantum;

fn pq(policy: PostQuantum) -> Offer {
    Offer::with_post_quantum(policy)
}

fn round_trip(sa: &mut Session, sb: &mut Session) {
    let frames = sa.seal(b"over the hybrid session", NOW).unwrap();
    let mut got = None;
    for frame in frames {
        got = sb.open(&frame, NOW).unwrap();
    }
    assert_eq!(got.unwrap(), b"over the hybrid session");
}

#[test]
fn hybrid_suite_is_selected_when_both_sides_offer_it() {
    for (first, expected) in [
        (
            Suite::HybridMlKem768ChaChaPoly,
            Suite::HybridMlKem768ChaChaPoly,
        ),
        (Suite::HybridMlKem768AesGcm, Suite::HybridMlKem768AesGcm),
    ] {
        let mut offer = pq(PostQuantum::Preferred);
        offer.suites.retain(|s| *s != first);
        offer.suites.insert(0, first);
        let a = Side::new(1, offer.clone());
        let b = Side::new(2, offer);
        let (mut sa, mut sb) = handshake(&a, &b).unwrap();
        assert_eq!(sa.selected().suite, expected);
        assert!(sa.selected().suite.hybrid());
        assert_eq!(sa.peer(), b.identity.node_id());
        assert_eq!(sb.peer(), a.identity.node_id());
        round_trip(&mut sa, &mut sb);
        round_trip(&mut sb, &mut sa);
    }
}

#[test]
fn hybrid_preferred_falls_back_to_classical_with_a_classical_peer() {
    let modern = Side::new(1, pq(PostQuantum::Preferred));
    let classical = Side::new(2, Offer::default());
    for (a, b) in [(&modern, &classical), (&classical, &modern)] {
        let (mut sa, mut sb) = handshake(a, b).unwrap();
        assert!(!sa.selected().suite.hybrid());
        round_trip(&mut sa, &mut sb);
    }
}

#[test]
fn hybrid_required_refuses_a_classical_peer() {
    let strict = Side::new(1, pq(PostQuantum::Required));
    let classical = Side::new(2, Offer::default());
    assert!(handshake(&strict, &classical).is_err());
    assert!(handshake(&classical, &strict).is_err());
    let other = Side::new(3, pq(PostQuantum::Preferred));
    assert!(
        handshake(&strict, &other)
            .unwrap()
            .0
            .selected()
            .suite
            .hybrid()
    );
}

/// Run a hybrid handshake, letting `tamper` edit message 1's Noise bytes.
fn hybrid_with_tampered_init(tamper: impl Fn(&mut Vec<u8>)) -> Result<(Session, Session)> {
    let a = Side::new(1, pq(PostQuantum::Required));
    let b = Side::new(2, pq(PostQuantum::Required));
    let (initiator, init) = Initiator::start(&a.config())?;
    let HandshakeMsg::Init {
        sid,
        offer,
        suite,
        noise,
    } = init
    else {
        unreachable!()
    };
    let mut bytes = B64.decode(noise).unwrap();
    tamper(&mut bytes);
    let noise = B64.encode(bytes);
    let ResponderStep::Continue(pending, HandshakeMsg::Resp { noise, .. }) =
        respond(&b.config(), &sid, &offer, suite, &noise)?
    else {
        unreachable!()
    };
    let (session_a, fin) = initiator.on_resp(&a.config(), &noise, NOW)?;
    let HandshakeMsg::Fin { noise, .. } = fin else {
        unreachable!()
    };
    Ok((session_a, pending.on_fin(&b.config(), &noise, NOW)?))
}

#[test]
fn the_ml_kem_key_is_bound_to_the_handshake() {
    // Untouched, the helper completes.
    assert!(hybrid_with_tampered_init(|_| {}).is_ok());
    // An attacker who swaps or alters the encapsulation key in flight
    // (to learn the ML-KEM secret) breaks the handshake instead.
    assert!(
        hybrid_with_tampered_init(|bytes| {
            let last = bytes.len() - 20;
            bytes[last] ^= 1;
        })
        .is_err()
    );
    assert!(
        hybrid_with_tampered_init(|bytes| {
            let (_, replacement) = crate::hybrid::generate();
            let start = bytes.len() - replacement.len();
            bytes[start..].copy_from_slice(&replacement);
        })
        .is_err()
    );
    // A truncated key is refused outright.
    assert!(hybrid_with_tampered_init(|bytes| bytes.truncate(40)).is_err());
}

#[test]
fn a_classical_handshake_must_not_carry_a_payload_in_message_one() {
    let a = Side::new(1, Offer::default());
    let b = Side::new(2, Offer::default());
    let (_, init) = Initiator::start(&a.config()).unwrap();
    let HandshakeMsg::Init {
        sid,
        offer,
        suite,
        noise,
    } = init
    else {
        unreachable!()
    };
    let mut bytes = B64.decode(noise).unwrap();
    bytes.extend_from_slice(&[7; 32]);
    assert!(respond(&b.config(), &sid, &offer, suite, &B64.encode(bytes)).is_err());
}

#[test]
fn hybrid_handshake_messages_fit_the_wire_limits() {
    let a = Side::new(1, pq(PostQuantum::Required));
    let b = Side::new(2, pq(PostQuantum::Required));
    let (initiator, init) = Initiator::start(&a.config()).unwrap();
    let HandshakeMsg::Init {
        sid,
        offer,
        suite,
        noise: init_noise,
    } = init
    else {
        unreachable!()
    };
    let ResponderStep::Continue(
        _,
        HandshakeMsg::Resp {
            noise: resp_noise, ..
        },
    ) = respond(&b.config(), &sid, &offer, suite, &init_noise).unwrap()
    else {
        unreachable!()
    };
    assert!(
        init_noise.len() <= MAX_HANDSHAKE_BYTES,
        "init is {}",
        init_noise.len()
    );
    assert!(
        resp_noise.len() <= MAX_HANDSHAKE_BYTES,
        "resp is {}",
        resp_noise.len()
    );
    drop(initiator);
}

#[test]
fn session_keys_depend_on_the_ml_kem_secret() {
    // Give the initiator a different ML-KEM secret after its key was sent.
    // Every X25519 value is unchanged and correct, yet the two sides now
    // hold different pre-shared keys, and message 3 must fail to open. So
    // knowing the X25519 secrets alone is not enough to complete or read
    // the session.
    let a = Side::new(1, pq(PostQuantum::Required));
    let b = Side::new(2, pq(PostQuantum::Required));
    let (mut initiator, init) = Initiator::start(&a.config()).unwrap();
    let HandshakeMsg::Init {
        sid,
        offer,
        suite,
        noise,
    } = init
    else {
        unreachable!()
    };
    let ResponderStep::Continue(pending, HandshakeMsg::Resp { noise, .. }) =
        respond(&b.config(), &sid, &offer, suite, &noise).unwrap()
    else {
        unreachable!()
    };
    initiator.kem = Some(crate::hybrid::generate().0);
    let (_, fin) = initiator.on_resp(&a.config(), &noise, NOW).unwrap();
    let HandshakeMsg::Fin { noise, .. } = fin else {
        unreachable!()
    };
    assert!(pending.on_fin(&b.config(), &noise, NOW).is_err());
}
