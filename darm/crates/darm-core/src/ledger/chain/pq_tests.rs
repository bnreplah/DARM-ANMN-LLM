use super::*;
use crate::ledger::{EntryBody, Feature, ValidatorSet, pq_bind_message};

const NOW: u64 = 1_700_000_000_000;

fn identity(seed: u8) -> NodeIdentity {
    NodeIdentity::from_secret_bytes(&[seed; 32])
}

fn ledger(active: bool) -> Ledger {
    let mut config = ChainConfig::new("pq", ValidatorSet::equal([identity(1).node_id()]).unwrap());
    if active {
        config.activations.insert(Feature::PqIdentity, 0);
    }
    Ledger::new(config)
}

/// A bind entry for `who`, proving possession with `prover`'s ML-DSA key.
fn bind(chain: Hash32, who: &NodeIdentity, prover: &NodeIdentity) -> SignedEntry {
    let pq = prover.pq_identity();
    let proof = pq.sign("pq-key-bind", &pq_bind_message(&chain, &who.node_id()));
    let body = EntryBody::PqKeyBind {
        key: pq.public().as_bytes().to_vec(),
        proof,
    };
    SignedEntry::sign(who, chain, body, NOW).unwrap()
}

fn finalize(ledger: &mut Ledger) -> bool {
    let v = identity(1);
    match ledger.propose(&v, 0, NOW).unwrap() {
        Some(block) => {
            ledger.attest(&v, &block.hash()).unwrap();
            true
        }
        None => false,
    }
}

#[test]
fn a_node_binds_its_ml_dsa_key_once() {
    let mut ledger = ledger(true);
    let chain = ledger.chain_id();
    let v = identity(1);
    assert!(ledger.submit(bind(chain, &v, &v), NOW).unwrap());
    assert!(finalize(&mut ledger));
    let bound = ledger.state().pq_key(&v.node_id()).expect("bound");
    assert_eq!(bound, v.pq_identity().public());

    // The entry survives JSON (hex fields) and replay.
    let json = serde_json::to_string(ledger.blocks()).unwrap();
    assert!(json.len() < 16_000, "{}", json.len());
    let blocks: Vec<Block> = serde_json::from_str(&json).unwrap();
    let replayed = Ledger::replay(ledger.config().clone(), blocks).unwrap();
    assert_eq!(replayed.state().pq_key(&v.node_id()), Some(bound));

    // A second bind for the same node is never included.
    let again = SignedEntry::sign(
        &v,
        chain,
        match &bind(chain, &v, &v).entry.body {
            body @ EntryBody::PqKeyBind { .. } => body.clone(),
            _ => unreachable!(),
        },
        NOW + 1,
    )
    .unwrap();
    ledger.submit(again, NOW + 1).unwrap();
    assert!(!finalize(&mut ledger));
}

#[test]
fn binds_need_possession_registration_and_activation() {
    let mut ledger = ledger(true);
    let chain = ledger.chain_id();
    let (v, other) = (identity(1), identity(2));

    // Someone else's key: the proof does not name this node.
    let stolen = {
        let pq = other.pq_identity();
        let proof = pq.sign("pq-key-bind", &pq_bind_message(&chain, &other.node_id()));
        let body = EntryBody::PqKeyBind {
            key: pq.public().as_bytes().to_vec(),
            proof,
        };
        SignedEntry::sign(&v, chain, body, NOW).unwrap()
    };
    assert!(ledger.submit(stolen, NOW).is_err());
    // A proof made for another chain.
    let foreign = bind(Hash32::digest("chain", b"other"), &v, &v);
    let body = foreign.entry.body.clone();
    let entry = SignedEntry::sign(&v, chain, body, NOW).unwrap();
    assert!(ledger.submit(entry, NOW).is_err());
    // Wrong sizes are malformed before any crypto runs.
    let short = EntryBody::PqKeyBind {
        key: vec![0; 100],
        proof: vec![0; 3309],
    };
    assert!(SignedEntry::sign(&v, chain, short, NOW).is_err());

    // An unregistered non-validator's bind is valid crypto but not applied.
    ledger.submit(bind(chain, &other, &other), NOW).unwrap();
    assert!(!finalize(&mut ledger));
    assert!(ledger.state().pq_key(&other.node_id()).is_none());

    // Feature not activated: refused.
    let mut off = super::Ledger::new(ledger_config_off());
    let err = off.submit(bind(off.chain_id(), &v, &v), NOW).unwrap_err();
    assert!(err.to_string().contains("pq-identity"), "{err}");
}

fn ledger_config_off() -> ChainConfig {
    ledger(false).config().clone()
}
