use super::*;
use crate::ledger::locking::{Vote, VoteKind, vote_message};
use crate::ledger::{EQUIVOCATION_PENALTY_MILLI, EntryBody, Feature, ValidatorSet};

const NOW: u64 = 1_700_000_000_000;

fn identity(seed: u8) -> NodeIdentity {
    NodeIdentity::from_secret_bytes(&[seed; 32])
}

fn vote(chain: &Hash32, who: &NodeIdentity, height: u64, block: Option<Hash32>) -> Vote {
    let message = vote_message(chain, VoteKind::Prevote, height, 0, block.as_ref());
    Vote {
        kind: VoteKind::Prevote,
        height,
        round: 0,
        block,
        validator: who.node_id(),
        signature: who.sign(&message),
    }
}

fn ledger(validators: &[&NodeIdentity], evidence_from: Option<u64>) -> Ledger {
    let mut config = ChainConfig::new(
        "evidence",
        ValidatorSet::equal(validators.iter().map(|v| v.node_id())).unwrap(),
    );
    if let Some(height) = evidence_from {
        config.activations.insert(Feature::Evidence, height);
    }
    Ledger::new(config)
}

/// Two conflicting prevotes in canonical order.
fn pair(chain: &Hash32, who: &NodeIdentity) -> (Vote, Vote) {
    let a = vote(chain, who, 1, Some(Hash32::digest("b", b"a")));
    let b = vote(chain, who, 1, Some(Hash32::digest("b", b"b")));
    if a.block < b.block { (a, b) } else { (b, a) }
}

fn evidence(
    chain: Hash32,
    author: &NodeIdentity,
    first: Vote,
    second: Vote,
) -> Result<SignedEntry> {
    SignedEntry::sign(
        author,
        chain,
        EntryBody::Equivocation { first, second },
        NOW,
    )
}

/// Finalize whatever is pending; returns whether a block was produced.
fn finalize(ledger: &mut Ledger, proposer: &NodeIdentity) -> bool {
    match ledger.propose(proposer, 0, NOW).unwrap() {
        Some(block) => {
            ledger.attest(proposer, &block.hash()).unwrap();
            true
        }
        None => false,
    }
}

#[test]
fn anyone_can_record_equivocation_and_it_costs_reputation_once() {
    let v = identity(1);
    let reporter = identity(9);
    let mut ledger = ledger(&[&v], Some(0));
    let chain = ledger.chain_id();
    let (first, second) = pair(&chain, &v);
    let entry = evidence(chain, &reporter, first.clone(), second.clone()).unwrap();
    assert!(ledger.submit(entry, NOW).unwrap());
    assert!(finalize(&mut ledger, &v));
    assert_eq!(ledger.state().equivocations(&v.node_id()), 1);
    assert_eq!(
        ledger.state().reputation(&v.node_id()),
        (crate::ledger::state::REPUTATION_MIN_MILLI).max(0 - EQUIVOCATION_PENALTY_MILLI)
    );
    // The same offence reported again (another author) does not apply twice.
    let again = evidence(chain, &identity(8), first, second).unwrap();
    ledger.submit(again, NOW).unwrap();
    assert!(!finalize(&mut ledger, &v), "duplicate is not included");
    assert_eq!(ledger.state().equivocations(&v.node_id()), 1);
}

#[test]
fn evidence_must_be_genuine_ordered_and_activated() {
    let v = identity(1);
    let other = identity(2);
    let mut ledger = ledger(&[&v], Some(0));
    let chain = ledger.chain_id();
    let (first, second) = pair(&chain, &v);

    // Reversed order and identical votes are malformed.
    assert!(evidence(chain, &other, second.clone(), first.clone()).is_err());
    assert!(evidence(chain, &other, first.clone(), first.clone()).is_err());
    // Different validators or heights do not prove anything.
    let mut mixed = second.clone();
    mixed.validator = other.node_id();
    assert!(evidence(chain, &other, first.clone(), mixed).is_err());
    let later = vote(&chain, &v, 2, second.block);
    assert!(evidence(chain, &other, first.clone(), later).is_err());

    // A forged second signature, or votes from another chain, are rejected.
    let mut forged = second.clone();
    forged.signature = other.sign(b"not the vote");
    let entry = evidence(chain, &other, first.clone(), forged).unwrap();
    assert!(ledger.submit(entry, NOW).is_err());
    let foreign = Hash32::digest("chain", b"elsewhere");
    let (f1, f2) = pair(&foreign, &v);
    let entry = evidence(chain, &other, f1, f2).unwrap();
    assert!(ledger.submit(entry, NOW).is_err());

    // A non-validator's double vote is not evidence against the chain.
    let (o1, o2) = pair(&chain, &other);
    let entry = evidence(chain, &v, o1, o2).unwrap();
    ledger.submit(entry, NOW).unwrap();
    assert!(!finalize(&mut ledger, &v), "dropped, so nothing to propose");
    assert_eq!(ledger.state().equivocations(&other.node_id()), 0);

    // Not activated: refused outright.
    let mut off = Ledger::new(ChainConfig::new(
        "evidence",
        ValidatorSet::equal([v.node_id()]).unwrap(),
    ));
    let entry = evidence(off.chain_id(), &other, first, second).unwrap();
    let err = off.submit(entry, NOW).unwrap_err();
    assert!(err.to_string().contains("evidence"), "{err}");
}
