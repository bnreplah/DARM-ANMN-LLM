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
