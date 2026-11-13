use super::*;
use crate::ledger::{
    EntryBody, Feature, Member, VALIDATOR_SET_KIND, ValidatorSet, validator_set_payload,
};

const NOW: u64 = 1_700_000_000_000;

fn identity(seed: u8) -> NodeIdentity {
    NodeIdentity::from_secret_bytes(&[seed; 32])
}

struct Chain {
    ledger: Ledger,
    ids: Vec<NodeIdentity>,
    clock: u64,
}

impl Chain {
    fn new(genesis: &[u8], governance: bool) -> Self {
        let ids: Vec<NodeIdentity> = (1..=4).map(identity).collect();
        let set = ValidatorSet::equal(genesis.iter().map(|s| identity(*s).node_id())).unwrap();
        let mut config = ChainConfig::new("governance", set);
        if governance {
            config.activations.insert(Feature::Governance, 0);
        }
        Self {
            ledger: Ledger::new(config),
            ids,
            clock: NOW,
        }
    }

    fn id(&self, seed: u8) -> &NodeIdentity {
        &self.ids[usize::from(seed - 1)]
    }

    /// Submit entries and finalize one block, attested by `signers` (seeds).
    /// Returns entry ids and whether the block committed.
    fn block(&mut self, entries: Vec<(u8, EntryBody)>, signers: &[u8]) -> (Vec<Hash32>, bool) {
        let chain = self.ledger.chain_id();
        let mut ids = Vec::new();
        for (who, body) in entries {
            let entry = SignedEntry::sign(self.id(who), chain, body, self.clock).unwrap();
            ids.push(entry.id());
            self.ledger.submit(entry, self.clock).unwrap();
        }
        let height = self.ledger.height();
        let proposer = self.ledger.proposer_for_next(0);
        let seed = (1..=4)
            .find(|s| identity(*s).node_id() == proposer)
            .unwrap();
        let block = self
            .ledger
            .propose(&identity(seed), 0, self.clock)
            .unwrap()
            .expect("something to propose");
        for s in signers {
            let _ = self.ledger.attest(&identity(*s), &block.hash());
        }
        self.clock += 1_000;
        (ids, self.ledger.height() > height)
    }

    fn heartbeat(&self, who: u8) -> (u8, EntryBody) {
        (
            who,
            EntryBody::Proposal {
                kind: "note".into(),
                payload: Hash32::digest("filler", &self.clock.to_be_bytes()),
                deadline_ms: self.clock + 1_000_000,
            },
        )
    }

    /// Propose and pass a validator-set change; returns the proposal id.
    fn pass(&mut self, members: &[Member], effective: u64, voters: &[u8]) -> Hash32 {
        let proposer = voters[0];
        let (ids, ok) = self.block(
            vec![(
                proposer,
                EntryBody::Proposal {
                    kind: VALIDATOR_SET_KIND.into(),
                    payload: validator_set_payload(members, effective),
                    deadline_ms: self.clock + 100_000_000,
                },
            )],
            voters,
        );
        assert!(ok);
        let votes = voters
            .iter()
            .map(|v| {
                (
                    *v,
                    EntryBody::Vote {
                        proposal: ids[0],
                        support: true,
                    },
                )
            })
            .collect();
        assert!(self.block(votes, voters).1);
        ids[0]
    }
}

fn members(seeds: &[u8]) -> Vec<Member> {
    let mut list: Vec<Member> = seeds
        .iter()
        .map(|s| Member {
            node: identity(*s).node_id(),
            weight: 1,
        })
        .collect();
    list.sort_by_key(|m| m.node);
    list
}

fn change(proposal: Hash32, members: Vec<Member>, effective_height: u64) -> EntryBody {
    EntryBody::ValidatorSetChange {
        proposal,
        members,
        effective_height,
    }
}

#[test]
fn a_governed_change_adds_a_validator_from_its_effective_height() {
    let mut chain = Chain::new(&[1, 2], true);
    let new_set = members(&[1, 2, 3]);
    let effective = chain.ledger.height() + 6;
    let proposal = chain.pass(&new_set, effective, &[1, 2]);
    assert!(
        chain
            .block(
                vec![(1, change(proposal, new_set.clone(), effective))],
                &[1, 2]
            )
            .1
    );
    assert_eq!(chain.ledger.validators().len(), 2, "not yet effective");
    while chain.ledger.height() + 1 < effective {
        let hb = chain.heartbeat(1);
        assert!(chain.block(vec![hb], &[1, 2]).1);
    }
    // From the effective height on, the new set must attest: 2 of 3 is not
    // a two-thirds quorum, all three are.
    assert_eq!(chain.ledger.validators().len(), 3);
    assert!(chain.ledger.is_validator(&identity(3).node_id()));
    let hb = chain.heartbeat(2);
    let (_, committed) = chain.block(vec![hb], &[1, 2]);
    assert!(!committed, "old quorum no longer enough");
    let candidate = *chain.ledger.candidates.keys().next().unwrap();
    chain.ledger.attest(&identity(3), &candidate).unwrap();
    assert_eq!(chain.ledger.height(), effective);

    // A fresh replica replays to the same sets and head.
    let replayed = Ledger::replay(
        chain.ledger.config().clone(),
        chain.ledger.blocks().to_vec(),
    )
    .unwrap();
    assert_eq!(replayed.head_hash(), chain.ledger.head_hash());
    assert_eq!(replayed.validators(), chain.ledger.validators());
}

#[test]
fn a_removed_validator_can_no_longer_attest() {
    let mut chain = Chain::new(&[1, 2, 3, 4], true);
    let new_set = members(&[1, 2, 3]);
    let effective = chain.ledger.height() + 6;
    let proposal = chain.pass(&new_set, effective, &[1, 2, 3, 4]);
    assert!(
        chain
            .block(
                vec![(2, change(proposal, new_set, effective))],
                &[1, 2, 3, 4]
            )
            .1
    );
    while chain.ledger.height() + 1 < effective {
        let hb = chain.heartbeat(1);
        assert!(chain.block(vec![hb], &[1, 2, 3, 4]).1);
    }
    assert!(!chain.ledger.is_validator(&identity(4).node_id()));
    let hb = chain.heartbeat(1);
    assert!(chain.block(vec![hb], &[1, 2, 3]).1);
    let hb = chain.heartbeat(2);
    let chain_id = chain.ledger.chain_id();
    let entry = SignedEntry::sign(chain.id(2), chain_id, hb.1, chain.clock).unwrap();
    chain.ledger.submit(entry, chain.clock).unwrap();
    let proposer = chain.ledger.proposer_for_next(0);
    assert_ne!(
        proposer,
        identity(4).node_id(),
        "removed validators never propose"
    );
    let seed = (1..=3)
        .find(|s| identity(*s).node_id() == proposer)
        .unwrap();
    let block = chain
        .ledger
        .propose(&identity(seed), 0, chain.clock)
        .unwrap()
        .unwrap();
    assert!(chain.ledger.attest(&identity(4), &block.hash()).is_err());
}

#[test]
fn changes_need_a_matching_passed_proposal_delay_and_activation() {
    let mut chain = Chain::new(&[1, 2], true);
    let new_set = members(&[1, 2, 3]);
    let effective = chain.ledger.height() + 10;
    let chain_id = chain.ledger.chain_id();
    let sign = |who: u8, body: EntryBody, clock: u64| {
        SignedEntry::sign(&identity(who), chain_id, body, clock)
    };

    // Unsorted or duplicate members are malformed.
    let mut unsorted = new_set.clone();
    unsorted.reverse();
    assert!(sign(1, change(Hash32::ZERO, unsorted, effective), chain.clock).is_err());
    let dup = vec![new_set[0], new_set[0]];
    assert!(sign(1, change(Hash32::ZERO, dup, effective), chain.clock).is_err());

    let proposal = chain.pass(&new_set, effective, &[1, 2]);
    let next = chain.ledger.height() + 1;
    // Too soon: refused at submission.
    let soon = sign(1, change(proposal, new_set.clone(), next + 1), chain.clock).unwrap();
    let err = chain.ledger.submit(soon, chain.clock).unwrap_err();
    assert!(err.to_string().contains("effective height"), "{err}");
    // Different members or height than approved, or a non-validator author:
    // dropped when proposing.
    for (who, body) in [
        (1, change(proposal, members(&[1, 3]), effective)),
        (1, change(proposal, new_set.clone(), effective + 1)),
        (3, change(proposal, new_set.clone(), effective)),
    ] {
        let entry = sign(who, body, chain.clock).unwrap();
        chain.ledger.submit(entry, chain.clock).unwrap();
    }
    let hb = chain.heartbeat(1);
    assert!(chain.block(vec![hb], &[1, 2]).1);
    assert_eq!(chain.ledger.state().validator_changes().count(), 0);

    // The matching change applies once.
    assert!(
        chain
            .block(
                vec![(1, change(proposal, new_set.clone(), effective))],
                &[1, 2]
            )
            .1
    );
    assert_eq!(chain.ledger.state().validator_changes().count(), 1);

    // Without the governance feature, refused outright.
    let mut off = Chain::new(&[1, 2], false);
    let id = off.ledger.chain_id();
    let entry = SignedEntry::sign(&identity(1), id, change(proposal, new_set, 50), NOW).unwrap();
    let err = off.ledger.submit(entry, NOW).unwrap_err();
    assert!(err.to_string().contains("governance"), "{err}");
}
