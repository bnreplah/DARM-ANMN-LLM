use super::*;
use crate::ledger::{CREDIT_GRANT_KIND, EntryBody, Payment, ValidatorSet, credit_grant_payload};

const NOW: u64 = 1_700_000_000_000;

fn identity(seed: u8) -> NodeIdentity {
    NodeIdentity::from_secret_bytes(&[seed; 32])
}

struct Harness {
    ledger: Ledger,
    v: NodeIdentity,
    clock: u64,
}

impl Harness {
    fn new() -> Self {
        let v = identity(1);
        let config = ChainConfig {
            name: "credits".into(),
            validators: ValidatorSet::equal([v.node_id()]).unwrap(),
        };
        Self {
            ledger: Ledger::new(config),
            v,
            clock: NOW,
        }
    }

    /// Submit entries in one block; returns each entry's id.
    fn block(&mut self, entries: Vec<(&NodeIdentity, EntryBody)>) -> Vec<Hash32> {
        let chain = self.ledger.chain_id();
        let mut ids = Vec::new();
        for (who, body) in entries {
            let entry = SignedEntry::sign(who, chain, body, self.clock).unwrap();
            ids.push(entry.id());
            self.ledger.submit(entry, self.clock).unwrap();
        }
        if let Some(block) = self.ledger.propose(&self.v, 0, self.clock).unwrap() {
            self.ledger.attest(&self.v, &block.hash()).unwrap();
        }
        self.clock += 1_000;
        ids
    }

    fn register(&mut self, who: &NodeIdentity) {
        let body = EntryBody::NodeRegistration {
            role: "worker".into(),
            genome: Hash32::digest("genome", &who.node_id().as_bytes()[..]),
            generation: 0,
            parent: None,
        };
        self.block(vec![(who, body)]);
    }

    /// Governance-approved grant: proposal, validator vote, grant entry.
    fn grant(&mut self, to: &NodeIdentity, amount: u64) {
        let v = identity(1);
        let id = self.block(vec![(
            &v,
            EntryBody::Proposal {
                kind: CREDIT_GRANT_KIND.into(),
                payload: credit_grant_payload(&to.node_id(), amount),
                deadline_ms: self.clock + 10_000_000,
            },
        )])[0];
        self.block(vec![(
            &v,
            EntryBody::Vote {
                proposal: id,
                support: true,
            },
        )]);
        self.block(vec![(
            &v,
            EntryBody::CreditGrant {
                proposal: id,
                to: to.node_id(),
                amount,
            },
        )]);
    }

    fn commit_job(&mut self, coordinator: &NodeIdentity, workers: u16) -> (Hash32, Hash32) {
        let job = Hash32::digest("job", &[workers as u8, coordinator.node_id().as_bytes()[0]]);
        let receipts_root = Hash32::digest("receipts", job.as_bytes());
        self.block(vec![(
            coordinator,
            EntryBody::ComputeCommit {
                job,
                result_root: Hash32::digest("result", job.as_bytes()),
                receipts_root,
                shards: 4,
                workers,
            },
        )]);
        (job, receipts_root)
    }

    fn balance(&self, who: &NodeIdentity) -> u64 {
        self.ledger.state().credit_balance(&who.node_id())
    }

    fn supply_conserved(&self) -> bool {
        let sum: u64 = self.ledger.state().credit_balances().map(|(_, b)| *b).sum();
        sum == self.ledger.state().credits_issued()
    }
}

fn pay(worker: &NodeIdentity, amount: u64) -> Payment {
    Payment {
        worker: worker.node_id(),
        amount,
    }
}

fn settle(job: Hash32, receipts_root: Hash32, payments: Vec<Payment>) -> EntryBody {
    EntryBody::ComputeSettle {
        job,
        receipts_root,
        payments,
    }
}

fn setup() -> (Harness, NodeIdentity, NodeIdentity, NodeIdentity) {
    let mut h = Harness::new();
    let (coord, w1, w2) = (identity(2), identity(3), identity(4));
    for who in [&coord, &w1, &w2] {
        h.register(who);
    }
    (h, coord, w1, w2)
}

#[test]
fn grant_then_settle_moves_credits_and_conserves_supply() {
    let (mut h, coord, w1, w2) = setup();
    h.grant(&coord, 100);
    assert_eq!(h.balance(&coord), 100);
    let (job, root) = h.commit_job(&coord, 2);
    h.block(vec![(
        &coord,
        settle(job, root, vec![pay(&w1, 30), pay(&w2, 20)]),
    )]);
    assert_eq!(h.balance(&coord), 50);
    assert_eq!(h.balance(&w1), 30);
    assert_eq!(h.balance(&w2), 20);
    assert_eq!(h.ledger.state().credits_issued(), 100);
    assert!(h.supply_conserved());
}

#[test]
fn settlement_rules_reject_abuse() {
    let (mut h, coord, w1, w2) = setup();
    h.grant(&coord, 100);
    let (job, root) = h.commit_job(&coord, 1);
    let outsider = identity(9);
    h.register(&outsider);
    h.grant(&outsider, 100);
    let before = h.ledger.height();

    // Unregistered payee, wrong receipts root, too many payees, overspend,
    // someone else's job, all dropped without changing balances.
    let ghost = identity(8);
    for (who, body) in [
        (&coord, settle(job, root, vec![pay(&ghost, 5)])),
        (&coord, settle(job, Hash32::ZERO, vec![pay(&w1, 5)])),
        (&coord, settle(job, root, vec![pay(&w1, 5), pay(&w2, 5)])),
        (&coord, settle(job, root, vec![pay(&w1, 101)])),
        (&outsider, settle(job, root, vec![pay(&w1, 5)])),
    ] {
        h.block(vec![(who, body)]);
    }
    assert_eq!(h.balance(&w1), 0);
    assert_eq!(h.balance(&coord), 100);
    assert_eq!(h.balance(&outsider), 100);
    assert!(h.ledger.height() >= before);

    // One settlement per job.
    h.block(vec![(&coord, settle(job, root, vec![pay(&w1, 40)]))]);
    h.block(vec![(&coord, settle(job, root, vec![pay(&w1, 40)]))]);
    assert_eq!(h.balance(&w1), 40);
    assert_eq!(h.balance(&coord), 60);
    assert!(h.supply_conserved());
}

#[test]
fn shape_rules_reject_self_payment_duplicates_and_zero() {
    let a = identity(2);
    let b = identity(3);
    let chain = Hash32::digest("c", b"");
    let job = Hash32::digest("j", b"");
    for payments in [
        vec![pay(&a, 1)],
        vec![pay(&b, 1), pay(&b, 2)],
        vec![pay(&b, 0)],
        vec![],
    ] {
        assert!(
            SignedEntry::sign(&a, chain, settle(job, job, payments), NOW).is_err(),
            "should reject"
        );
    }
    let grant = |amount| EntryBody::CreditGrant {
        proposal: job,
        to: b.node_id(),
        amount,
    };
    assert!(SignedEntry::sign(&a, chain, grant(0), NOW).is_err());
    assert!(
        SignedEntry::sign(&a, chain, grant(crate::ledger::MAX_CREDIT_AMOUNT + 1), NOW).is_err()
    );
}

#[test]
fn grants_need_a_passed_matching_validator_proposal_once() {
    let (mut h, coord, _w1, _w2) = setup();
    let v = identity(1);

    // Non-validator cannot grant even with a passing proposal.
    let prop = h.block(vec![(
        &v,
        EntryBody::Proposal {
            kind: CREDIT_GRANT_KIND.into(),
            payload: credit_grant_payload(&coord.node_id(), 50),
            deadline_ms: h.clock + 10_000_000,
        },
    )])[0];
    // Not yet voted: grant is dropped.
    h.block(vec![(
        &v,
        EntryBody::CreditGrant {
            proposal: prop,
            to: coord.node_id(),
            amount: 50,
        },
    )]);
    assert_eq!(h.balance(&coord), 0);

    h.block(vec![(
        &v,
        EntryBody::Vote {
            proposal: prop,
            support: true,
        },
    )]);
    // Amount or recipient differing from the approved payload is dropped.
    h.block(vec![(
        &v,
        EntryBody::CreditGrant {
            proposal: prop,
            to: coord.node_id(),
            amount: 51,
        },
    )]);
    h.block(vec![(
        &coord,
        EntryBody::CreditGrant {
            proposal: prop,
            to: coord.node_id(),
            amount: 50,
        },
    )]);
    assert_eq!(h.balance(&coord), 0);

    // Matching grant works exactly once.
    for _ in 0..2 {
        h.block(vec![(
            &v,
            EntryBody::CreditGrant {
                proposal: prop,
                to: coord.node_id(),
                amount: 50,
            },
        )]);
    }
    assert_eq!(h.balance(&coord), 50);
    assert_eq!(h.ledger.state().credits_issued(), 50);
    assert!(h.supply_conserved());
}

#[test]
fn replay_reproduces_balances() {
    let (mut h, coord, w1, _w2) = setup();
    h.grant(&coord, 10);
    let (job, root) = h.commit_job(&coord, 1);
    h.block(vec![(&coord, settle(job, root, vec![pay(&w1, 7)]))]);
    let replayed = Ledger::replay(h.ledger.config().clone(), h.ledger.blocks().to_vec()).unwrap();
    assert_eq!(replayed.state().credit_balance(&w1.node_id()), 7);
    assert_eq!(replayed.state().credit_balance(&coord.node_id()), 3);
}
