use super::*;
use std::collections::VecDeque;

const HEIGHT: u64 = 7;

fn chain() -> Hash32 {
    Hash32::digest("test-chain", b"locking")
}

fn identities(n: u8) -> Vec<NodeIdentity> {
    (1..=n)
        .map(|seed| NodeIdentity::from_secret_bytes(&[seed; 32]))
        .collect()
}

fn set(ids: &[NodeIdentity]) -> ValidatorSet {
    ValidatorSet::equal(ids.iter().map(NodeIdentity::node_id)).unwrap()
}

fn block(tag: &str) -> Hash32 {
    Hash32::digest("test-block", tag.as_bytes())
}

fn sign_vote(id: &NodeIdentity, kind: VoteKind, round: u32, block: Option<Hash32>) -> Vote {
    Vote {
        kind,
        height: HEIGHT,
        round,
        block,
        validator: id.node_id(),
        signature: id.sign(&vote_message(&chain(), kind, HEIGHT, round, block.as_ref())),
    }
}

fn sign_proposal(
    id: &NodeIdentity,
    round: u32,
    block: Hash32,
    valid_round: Option<u32>,
) -> Proposal {
    Proposal {
        height: HEIGHT,
        round,
        block,
        valid_round,
        proposer: id.node_id(),
        signature: id.sign(&proposal_message(
            &chain(),
            HEIGHT,
            round,
            &block,
            valid_round,
        )),
    }
}

/// The identity that proposes `round`.
fn proposer(ids: &[NodeIdentity], round: u32) -> &NodeIdentity {
    let node = set(ids).proposer_for(HEIGHT, round);
    ids.iter().find(|id| id.node_id() == node).unwrap()
}

fn broadcasts(actions: &[Action]) -> Vec<Vote> {
    actions
        .iter()
        .filter_map(|a| match a {
            Action::Broadcast(vote) => Some(vote.clone()),
            _ => None,
        })
        .collect()
}

// ---------------------------------------------------------------- rules

#[test]
fn honest_round_decides_with_a_verifiable_certificate() {
    let ids = identities(4);
    let me = &ids[0];
    let mut hv = HeightVote::new(chain(), HEIGHT, set(&ids));
    let a = block("a");
    let actions = hv
        .on_proposal(Some(me), sign_proposal(proposer(&ids, 0), 0, a, None), true)
        .unwrap();
    assert_eq!(
        broadcasts(&actions),
        [sign_vote(me, VoteKind::Prevote, 0, Some(a))]
    );

    // Two more prevotes make a polka: lock and precommit.
    hv.on_vote(Some(me), sign_vote(&ids[1], VoteKind::Prevote, 0, Some(a)))
        .unwrap();
    let actions = hv
        .on_vote(Some(me), sign_vote(&ids[2], VoteKind::Prevote, 0, Some(a)))
        .unwrap();
    assert_eq!(
        broadcasts(&actions),
        [sign_vote(me, VoteKind::Precommit, 0, Some(a))]
    );
    assert_eq!(hv.locked(), Some((0, a)));
    assert_eq!(hv.valid(), Some((0, a)));

    hv.on_vote(
        Some(me),
        sign_vote(&ids[1], VoteKind::Precommit, 0, Some(a)),
    )
    .unwrap();
    let actions = hv
        .on_vote(
            Some(me),
            sign_vote(&ids[2], VoteKind::Precommit, 0, Some(a)),
        )
        .unwrap();
    let Some(Action::Commit {
        block,
        round,
        precommits,
    }) = actions.last()
    else {
        panic!("expected a commit, got {actions:?}");
    };
    assert_eq!((*block, *round), (a, 0));
    assert_eq!(hv.decided(), Some(a));
    // The certificate stands on its own: quorum weight, valid signatures.
    assert!(set(&ids).has_quorum(precommits.iter().map(|v| &v.validator)));
    for vote in precommits {
        let message = vote_message(&chain(), VoteKind::Precommit, HEIGHT, 0, Some(&a));
        vote.validator.verify(&message, &vote.signature).unwrap();
    }
}

#[test]
fn two_of_four_is_not_a_quorum() {
    let ids = identities(4);
    let mut hv = HeightVote::new(chain(), HEIGHT, set(&ids));
    let a = block("a");
    hv.on_proposal(None, sign_proposal(proposer(&ids, 0), 0, a, None), true)
        .unwrap();
    hv.on_vote(None, sign_vote(&ids[1], VoteKind::Prevote, 0, Some(a)))
        .unwrap();
    hv.on_vote(None, sign_vote(&ids[2], VoteKind::Prevote, 0, Some(a)))
        .unwrap();
    assert_eq!(hv.locked(), None);
    hv.on_vote(None, sign_vote(&ids[1], VoteKind::Precommit, 0, Some(a)))
        .unwrap();
    hv.on_vote(None, sign_vote(&ids[2], VoteKind::Precommit, 0, Some(a)))
        .unwrap();
    assert_eq!(hv.decided(), None);
}

#[test]
fn a_locked_validator_refuses_a_different_block() {
    let ids = identities(4);
    let me = &ids[0];
    let mut hv = HeightVote::new(chain(), HEIGHT, set(&ids));
    let (a, c) = (block("a"), block("c"));
    hv.on_proposal(Some(me), sign_proposal(proposer(&ids, 0), 0, a, None), true)
        .unwrap();
    hv.on_vote(Some(me), sign_vote(&ids[1], VoteKind::Prevote, 0, Some(a)))
        .unwrap();
    hv.on_vote(Some(me), sign_vote(&ids[2], VoteKind::Prevote, 0, Some(a)))
        .unwrap();
    assert_eq!(hv.locked(), Some((0, a)));
    // No decision in round 0; move on.
    let actions = hv.on_timeout(Some(me), 0, Step::Precommit);
    assert_eq!(actions, [Action::EnterRound(1)]);

    // Round 1 proposes a fresh block: a locked validator prevotes nil.
    let actions = hv
        .on_proposal(Some(me), sign_proposal(proposer(&ids, 1), 1, c, None), true)
        .unwrap();
    assert_eq!(
        broadcasts(&actions),
        [sign_vote(me, VoteKind::Prevote, 1, None)]
    );
    assert_eq!(hv.locked(), Some((0, a)), "the lock survives");
}

#[test]
fn a_lock_is_released_only_by_a_newer_polka_the_validator_has_seen() {
    let ids = identities(4);
    let me = &ids[0];
    let (a, d) = (block("a"), block("d"));
    let locked_on_a = || {
        let mut hv = HeightVote::new(chain(), HEIGHT, set(&ids));
        hv.on_proposal(Some(me), sign_proposal(proposer(&ids, 0), 0, a, None), true)
            .unwrap();
        hv.on_vote(Some(me), sign_vote(&ids[1], VoteKind::Prevote, 0, Some(a)))
            .unwrap();
        hv.on_vote(Some(me), sign_vote(&ids[2], VoteKind::Prevote, 0, Some(a)))
            .unwrap();
        hv.on_timeout(Some(me), 0, Step::Precommit);
        // Round 1 passes without this node seeing a proposal.
        hv.on_timeout(Some(me), 1, Step::Propose);
        hv.on_timeout(Some(me), 1, Step::Prevote);
        hv.on_timeout(Some(me), 1, Step::Precommit);
        assert_eq!((hv.round(), hv.locked()), (2, Some((0, a))));
        hv
    };
    let reproposal = sign_proposal(proposer(&ids, 2), 2, d, Some(1));

    // Claimed polka for D in round 1 that this node never saw: it waits,
    // and on timeout prevotes nil.
    let mut hv = locked_on_a();
    assert!(
        hv.on_proposal(Some(me), reproposal.clone(), true)
            .unwrap()
            .is_empty()
    );
    let actions = hv.on_timeout(Some(me), 2, Step::Propose);
    assert_eq!(
        broadcasts(&actions),
        [sign_vote(me, VoteKind::Prevote, 2, None)]
    );

    // Same proposal, but the round-1 polka for D arrives: the newer polka
    // releases the round-0 lock.
    let mut hv = locked_on_a();
    for id in &ids[1..4] {
        hv.on_vote(Some(me), sign_vote(id, VoteKind::Prevote, 1, Some(d)))
            .unwrap();
    }
    let actions = hv.on_proposal(Some(me), reproposal.clone(), true).unwrap();
    assert_eq!(
        broadcasts(&actions),
        [sign_vote(me, VoteKind::Prevote, 2, Some(d))]
    );

    // The polka may also arrive after the proposal.
    let mut hv = locked_on_a();
    hv.on_proposal(Some(me), reproposal, true).unwrap();
    let mut sent = Vec::new();
    for id in &ids[1..4] {
        sent.extend(broadcasts(
            &hv.on_vote(Some(me), sign_vote(id, VoteKind::Prevote, 1, Some(d)))
                .unwrap(),
        ));
    }
    assert_eq!(sent, [sign_vote(me, VoteKind::Prevote, 2, Some(d))]);

    // A polka from a round older than the lock does not release it.
    let mut hv = HeightVote::new(chain(), HEIGHT, set(&ids));
    hv.on_timeout(Some(me), 0, Step::Propose);
    hv.on_timeout(Some(me), 0, Step::Prevote);
    hv.on_timeout(Some(me), 0, Step::Precommit);
    hv.on_proposal(Some(me), sign_proposal(proposer(&ids, 1), 1, a, None), true)
        .unwrap();
    for id in &ids[1..3] {
        hv.on_vote(Some(me), sign_vote(id, VoteKind::Prevote, 1, Some(a)))
            .unwrap();
    }
    assert_eq!(hv.locked(), Some((1, a)));
    hv.on_timeout(Some(me), 1, Step::Precommit);
    for id in &ids[1..4] {
        hv.on_vote(Some(me), sign_vote(id, VoteKind::Prevote, 0, Some(d)))
            .unwrap();
    }
    let actions = hv
        .on_proposal(
            Some(me),
            sign_proposal(proposer(&ids, 2), 2, d, Some(0)),
            true,
        )
        .unwrap();
    assert_eq!(
        broadcasts(&actions),
        [sign_vote(me, VoteKind::Prevote, 2, None)]
    );
}

#[test]
fn a_proposer_must_repropose_the_block_with_a_polka() {
    let ids = identities(4);
    let next = proposer(&ids, 1);
    let mut hv = HeightVote::new(chain(), HEIGHT, set(&ids));
    let a = block("a");
    hv.on_proposal(
        Some(next),
        sign_proposal(proposer(&ids, 0), 0, a, None),
        true,
    )
    .unwrap();
    for id in ids
        .iter()
        .filter(|id| id.node_id() != next.node_id())
        .take(2)
    {
        hv.on_vote(Some(next), sign_vote(id, VoteKind::Prevote, 0, Some(a)))
            .unwrap();
    }
    assert_eq!(hv.valid(), Some((0, a)));
    hv.on_timeout(Some(next), 0, Step::Precommit);
    let proposal = hv.propose(next, block("fresh")).unwrap();
    assert_eq!((proposal.block, proposal.valid_round), (a, Some(0)));
    // Only the round's proposer may sign.
    let other = ids
        .iter()
        .find(|id| id.node_id() != next.node_id())
        .unwrap();
    assert!(hv.propose(other, block("fresh")).is_err());
}

#[test]
fn an_unvalidated_block_gets_nil_and_is_never_locked() {
    let ids = identities(4);
    let me = &ids[0];
    let mut hv = HeightVote::new(chain(), HEIGHT, set(&ids));
    let a = block("a");
    let actions = hv
        .on_proposal(
            Some(me),
            sign_proposal(proposer(&ids, 0), 0, a, None),
            false,
        )
        .unwrap();
    assert_eq!(
        broadcasts(&actions),
        [sign_vote(me, VoteKind::Prevote, 0, None)]
    );
    for id in &ids[1..4] {
        hv.on_vote(Some(me), sign_vote(id, VoteKind::Prevote, 0, Some(a)))
            .unwrap();
    }
    assert_eq!((hv.locked(), hv.valid()), (None, None));
    // It still follows a decision the rest of the network reaches.
    for id in &ids[1..4] {
        hv.on_vote(Some(me), sign_vote(id, VoteKind::Precommit, 0, Some(a)))
            .unwrap();
    }
    assert_eq!(hv.decided(), Some(a));
}

#[test]
fn a_node_never_signs_twice_in_a_round_even_after_replay() {
    let ids = identities(4);
    let me = &ids[0];
    let a = block("a");
    // After a restart the node replays its own persisted prevote for nil.
    let mut hv = HeightVote::new(chain(), HEIGHT, set(&ids));
    hv.on_vote(Some(me), sign_vote(me, VoteKind::Prevote, 0, None))
        .unwrap();
    let actions = hv
        .on_proposal(Some(me), sign_proposal(proposer(&ids, 0), 0, a, None), true)
        .unwrap();
    assert!(broadcasts(&actions).is_empty(), "no second prevote");
}

#[test]
fn observers_and_non_validators_never_vote() {
    let ids = identities(4);
    let outsider = NodeIdentity::from_secret_bytes(&[99; 32]);
    let a = block("a");
    for identity in [None, Some(&outsider)] {
        let mut hv = HeightVote::new(chain(), HEIGHT, set(&ids));
        let mut actions = hv
            .on_proposal(identity, sign_proposal(proposer(&ids, 0), 0, a, None), true)
            .unwrap();
        for id in &ids[0..3] {
            actions.extend(
                hv.on_vote(identity, sign_vote(id, VoteKind::Prevote, 0, Some(a)))
                    .unwrap(),
            );
            actions.extend(
                hv.on_vote(identity, sign_vote(id, VoteKind::Precommit, 0, Some(a)))
                    .unwrap(),
            );
        }
        assert!(broadcasts(&actions).is_empty());
        assert_eq!(hv.decided(), Some(a));
    }
}

#[test]
fn invalid_messages_are_rejected() {
    let ids = identities(4);
    let outsider = NodeIdentity::from_secret_bytes(&[99; 32]);
    let mut hv = HeightVote::new(chain(), HEIGHT, set(&ids));
    let a = block("a");

    assert!(matches!(
        hv.on_vote(None, sign_vote(&outsider, VoteKind::Prevote, 0, Some(a))),
        Err(DarmError::Unauthorized(_))
    ));
    let mut wrong_height = sign_vote(&ids[1], VoteKind::Prevote, 0, Some(a));
    wrong_height.height += 1;
    assert!(hv.on_vote(None, wrong_height).is_err());
    // A prevote signature does not work as a precommit, for another round,
    // or for another block.
    let mut forged = sign_vote(&ids[1], VoteKind::Prevote, 0, Some(a));
    forged.kind = VoteKind::Precommit;
    assert_eq!(hv.on_vote(None, forged), Err(DarmError::InvalidSignature));
    let mut forged = sign_vote(&ids[1], VoteKind::Prevote, 0, Some(a));
    forged.round = 1;
    assert_eq!(hv.on_vote(None, forged), Err(DarmError::InvalidSignature));
    let mut forged = sign_vote(&ids[1], VoteKind::Prevote, 0, Some(a));
    forged.block = None;
    assert_eq!(hv.on_vote(None, forged), Err(DarmError::InvalidSignature));
    assert!(matches!(
        hv.on_vote(
            None,
            sign_vote(&ids[1], VoteKind::Prevote, MAX_ROUND_LOOKAHEAD + 1, None)
        ),
        Err(DarmError::LimitExceeded(_))
    ));

    let not_proposer = ids
        .iter()
        .find(|id| id.node_id() != proposer(&ids, 0).node_id())
        .unwrap();
    assert!(matches!(
        hv.on_proposal(None, sign_proposal(not_proposer, 0, a, None), true),
        Err(DarmError::Unauthorized(_))
    ));
    assert!(
        hv.on_proposal(None, sign_proposal(proposer(&ids, 1), 1, a, Some(1)), true)
            .is_err()
    );
    hv.on_proposal(None, sign_proposal(proposer(&ids, 0), 0, a, None), true)
        .unwrap();
    assert!(
        hv.on_proposal(None, sign_proposal(proposer(&ids, 0), 0, a, None), true)
            .unwrap()
            .is_empty()
    );
    assert!(
        hv.on_proposal(
            None,
            sign_proposal(proposer(&ids, 0), 0, block("b"), None),
            true
        )
        .is_err()
    );
}

#[test]
fn double_votes_are_reported_and_counted_once() {
    let ids = identities(4);
    let mut hv = HeightVote::new(chain(), HEIGHT, set(&ids));
    let (a, b) = (block("a"), block("b"));
    let first = sign_vote(&ids[1], VoteKind::Prevote, 0, Some(a));
    let second = sign_vote(&ids[1], VoteKind::Prevote, 0, Some(b));
    assert!(hv.on_vote(None, first.clone()).unwrap().is_empty());
    assert!(
        hv.on_vote(None, first.clone()).unwrap().is_empty(),
        "duplicate"
    );
    assert_eq!(
        hv.on_vote(None, second.clone()).unwrap(),
        [Action::Equivocation(Box::new((first, second)))]
    );
}

#[test]
fn a_node_follows_the_network_to_a_later_round() {
    let ids = identities(4);
    let mut hv = HeightVote::new(chain(), HEIGHT, set(&ids));
    // One of four is not more than a third.
    assert!(
        hv.on_vote(None, sign_vote(&ids[1], VoteKind::Prevote, 3, None))
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        hv.on_vote(None, sign_vote(&ids[2], VoteKind::Prevote, 3, None))
            .unwrap(),
        [Action::EnterRound(3)]
    );
    assert_eq!((hv.round(), hv.step()), (3, Step::Propose));
    // Stale timers do nothing.
    assert!(hv.on_timeout(None, 0, Step::Propose).is_empty());
    assert_eq!(hv.round(), 3);
}

// ----------------------------------------------------------- simulation

/// Deterministic generator so failures reproduce from the seed alone.
struct Lcg(u64);

impl Lcg {
    fn next(&mut self) -> u64 {
        self.0 = self
            .0
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        self.0 >> 33
    }

    fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }
}

#[derive(Clone)]
enum Msg {
    Proposal(Proposal, Option<Vec<Vote>>),
    Vote(Vote),
    Commit(Hash32, u32, Vec<Vote>),
}

struct Sim {
    ids: std::rc::Rc<Vec<NodeIdentity>>,
    nodes: Vec<HeightVote>,
    byzantine: Vec<bool>,
    queue: VecDeque<(usize, Msg)>,
    rng: Lcg,
    drop_percent: usize,
}

impl Sim {
    fn new(n: u8, faulty: usize, seed: u64, drop_percent: usize) -> Self {
        let ids = std::rc::Rc::new(identities(n));
        let mut rng = Lcg(seed);
        let mut byzantine = vec![false; ids.len()];
        // Always corrupt the round-0 proposer first: the worst case.
        let first = proposer(&ids, 0).node_id();
        let mut left = faulty;
        if left > 0 {
            byzantine[ids.iter().position(|id| id.node_id() == first).unwrap()] = true;
            left -= 1;
        }
        while left > 0 {
            let pick = rng.below(ids.len());
            if !byzantine[pick] {
                byzantine[pick] = true;
                left -= 1;
            }
        }
        let nodes = ids
            .iter()
            .map(|_| HeightVote::new(chain(), HEIGHT, set(&ids)))
            .collect();
        let mut sim = Self {
            ids,
            nodes,
            byzantine,
            queue: VecDeque::new(),
            rng,
            drop_percent,
        };
        for index in 0..sim.ids.len() {
            sim.maybe_propose(index);
        }
        sim
    }

    fn honest(&self) -> impl Iterator<Item = usize> + '_ {
        (0..self.ids.len()).filter(|i| !self.byzantine[*i])
    }

    fn send(&mut self, to: usize, msg: Msg) {
        if self.rng.below(100) >= self.drop_percent {
            self.queue.push_back((to, msg));
        }
    }

    /// A Byzantine node tells half the network one thing and half another.
    fn split(&mut self, from: usize, round: u32, x: Option<Hash32>, y: Option<Hash32>) {
        let ids = self.ids.clone();
        let id = &ids[from];
        for to in 0..self.ids.len() {
            let block = if to % 2 == 0 { x } else { y };
            for kind in [VoteKind::Prevote, VoteKind::Precommit] {
                self.send(to, Msg::Vote(sign_vote(id, kind, round, block)));
            }
        }
    }

    fn maybe_propose(&mut self, index: usize) {
        let round = self.nodes[index].round();
        let ids = self.ids.clone();
        let id = &ids[index];
        if self.nodes[index].proposer(round) != id.node_id() {
            return;
        }
        if self.byzantine[index] {
            let (x, y) = (
                block(&format!("x{index}r{round}")),
                block(&format!("y{index}r{round}")),
            );
            for to in 0..self.ids.len() {
                let block = if to % 2 == 0 { x } else { y };
                self.send(
                    to,
                    Msg::Proposal(sign_proposal(id, round, block, None), None),
                );
            }
            self.split(index, round, Some(x), Some(y));
            return;
        }
        let fresh = block(&format!("honest{index}r{round}"));
        let proposal = self.nodes[index].propose(id, fresh).unwrap();
        let polka = proposal.valid_round.map(|round| {
            self.nodes[index]
                .polka(round, &proposal.block)
                .expect("a valid block always has a stored polka")
                .to_vec()
        });
        for to in 0..self.ids.len() {
            self.send(to, Msg::Proposal(proposal.clone(), polka.clone()));
        }
    }

    fn handle(&mut self, index: usize, actions: Vec<Action>) {
        for action in actions {
            match action {
                Action::Broadcast(vote) => {
                    for to in 0..self.ids.len() {
                        if to != index {
                            self.send(to, Msg::Vote(vote.clone()));
                        }
                    }
                }
                Action::EnterRound(_) => self.maybe_propose(index),
                // Deciders relay their certificate, as the module requires.
                Action::Commit {
                    block,
                    round,
                    precommits,
                } => {
                    for to in 0..self.ids.len() {
                        if to != index {
                            self.send(to, Msg::Commit(block, round, precommits.clone()));
                        }
                    }
                }
                Action::Equivocation(_) => {}
            }
        }
    }

    fn deliver(&mut self, to: usize, msg: Msg) {
        if self.byzantine[to] {
            // Echo any proposal as a split vote, then stay quiet.
            if let Msg::Proposal(p, _) = msg {
                self.split(to, p.round, Some(p.block), None);
            }
            return;
        }
        let ids = self.ids.clone();
        let id = &ids[to];
        let result = match msg {
            Msg::Proposal(p, polka) => {
                let mut actions = Vec::new();
                if let (Some(round), Some(polka)) = (p.valid_round, polka) {
                    actions = self.nodes[to]
                        .on_polka(Some(id), p.block, round, &polka)
                        .unwrap_or_default();
                }
                self.nodes[to].on_proposal(Some(id), p, true).map(|more| {
                    actions.extend(more);
                    actions
                })
            }
            Msg::Vote(v) => self.nodes[to].on_vote(Some(id), v),
            Msg::Commit(block, round, precommits) => {
                // A certificate for a block other than one already decided
                // would be a safety failure, so it must never verify.
                let result = self.nodes[to].on_commit(block, round, &precommits);
                assert!(
                    !matches!(&result, Err(DarmError::Rejected(m)) if m.starts_with("conflicting")),
                    "two certificates for different blocks"
                );
                result.map(|()| Vec::new())
            }
        };
        // Rejections (conflicting proposals, far rounds) are expected here.
        if let Ok(actions) = result {
            self.handle(to, actions);
        }
    }

    fn timeout(&mut self, index: usize) {
        let ids = self.ids.clone();
        let (round, step) = (self.nodes[index].round(), self.nodes[index].step());
        let actions = self.nodes[index].on_timeout(Some(&ids[index]), round, step);
        self.handle(index, actions);
    }

    /// Run with arbitrary reordering and, while `chaos` lasts, timers that
    /// fire at random. Afterwards timers fire only when the network is idle,
    /// which models eventual synchrony.
    fn run(&mut self, chaos: usize, budget: usize) {
        for tick in 0..budget {
            if self.honest().all(|i| self.nodes[i].decided().is_some()) {
                return;
            }
            if tick < chaos && self.rng.below(10) == 0 {
                let index = self.rng.below(self.ids.len());
                if !self.byzantine[index] {
                    self.timeout(index);
                }
                continue;
            }
            if self.queue.is_empty() {
                let waiting: Vec<usize> = self.honest().collect();
                for index in waiting {
                    self.timeout(index);
                }
                continue;
            }
            let pick = self.rng.below(self.queue.len());
            let (to, msg) = self.queue.remove(pick).unwrap();
            self.deliver(to, msg);
        }
    }

    fn decisions(&self) -> Vec<Option<Hash32>> {
        self.honest().map(|i| self.nodes[i].decided()).collect()
    }

    fn assert_agreement(&self, seed: u64) {
        let mut decided: Vec<Hash32> = self.decisions().into_iter().flatten().collect();
        decided.dedup();
        assert!(
            decided.len() <= 1,
            "seed {seed}: honest nodes disagree: {decided:?}"
        );
    }
}

#[test]
fn an_equivocating_proposer_cannot_stall_the_height() {
    // Under one-vote-per-height this stalls for good: the round-0 proposer
    // sends different blocks to different validators and the vote splits.
    let mut sim = Sim::new(4, 1, 1, 0);
    sim.run(0, 5_000);
    let decisions = sim.decisions();
    assert!(
        decisions.iter().all(Option::is_some),
        "all honest nodes decide"
    );
    sim.assert_agreement(1);
    assert!(
        sim.honest().all(|i| sim.nodes[i].round() >= 1),
        "round 0 failed"
    );
}

fn safety_sweep(seeds: std::ops::Range<u64>, configs: &[(u8, usize)]) {
    for seed in seeds {
        for &(n, faulty) in configs {
            let mut sim = Sim::new(n, faulty, seed, 20);
            sim.run(600, 1_500);
            sim.assert_agreement(seed);
        }
    }
}

fn liveness_sweep(seeds: std::ops::Range<u64>, configs: &[(u8, usize)]) {
    for seed in seeds {
        for &(n, faulty) in configs {
            let mut sim = Sim::new(n, faulty, seed, 0);
            sim.run(100, 50_000);
            sim.assert_agreement(seed);
            assert!(
                sim.decisions().iter().all(Option::is_some),
                "seed {seed}, n {n}: not every honest node decided"
            );
        }
    }
}

#[test]
fn honest_nodes_never_disagree_under_attack_reordering_and_loss() {
    safety_sweep(0..3, &[(4, 1)]);
}

#[test]
fn honest_nodes_decide_once_the_network_settles() {
    liveness_sweep(0..3, &[(4, 1)]);
}

/// The wide sweeps sign and verify millions of votes, which is slow without
/// optimizations: `cargo test --release -p darm-core --lib locking -- --ignored`.
#[test]
#[ignore = "slow in debug builds; run with --release"]
fn safety_sweep_wide() {
    safety_sweep(0..300, &[(4, 1), (7, 2), (10, 3)]);
}

#[test]
#[ignore = "slow in debug builds; run with --release"]
fn liveness_sweep_wide() {
    liveness_sweep(0..300, &[(4, 1), (7, 2), (10, 3)]);
}

#[test]
fn commit_certificates_are_verified_independently() {
    let ids = identities(4);
    let a = block("a");
    let cert: Vec<Vote> = ids[0..3]
        .iter()
        .map(|id| sign_vote(id, VoteKind::Precommit, 2, Some(a)))
        .collect();
    let fresh = || HeightVote::new(chain(), HEIGHT, set(&ids));

    // Accepted even when this node was shown a conflicting precommit by
    // one of the signers.
    let mut hv = fresh();
    hv.on_vote(
        None,
        sign_vote(&ids[2], VoteKind::Precommit, 2, Some(block("b"))),
    )
    .unwrap();
    hv.on_commit(a, 2, &cert).unwrap();
    assert_eq!(hv.decided(), Some(a));

    // Too few signers, repeated signers, wrong round, wrong block, wrong
    // kind and a bad signature are all refused.
    assert!(fresh().on_commit(a, 2, &cert[0..2]).is_err());
    let repeated = vec![cert[0].clone(), cert[0].clone(), cert[1].clone()];
    assert!(fresh().on_commit(a, 2, &repeated).is_err());
    assert!(fresh().on_commit(a, 1, &cert).is_err());
    assert!(fresh().on_commit(block("b"), 2, &cert).is_err());
    let prevotes: Vec<Vote> = ids[0..3]
        .iter()
        .map(|id| sign_vote(id, VoteKind::Prevote, 2, Some(a)))
        .collect();
    assert!(fresh().on_commit(a, 2, &prevotes).is_err());
    let mut forged = cert.clone();
    forged[0].signature = forged[1].signature;
    assert_eq!(
        fresh().on_commit(a, 2, &forged),
        Err(DarmError::InvalidSignature)
    );
    let oversized = vec![cert[0].clone(); 5];
    assert!(matches!(
        fresh().on_commit(a, 2, &oversized),
        Err(DarmError::LimitExceeded(_))
    ));
}

#[test]
fn agreement_can_break_with_a_third_faulty() {
    // The bound is tight: with 2 of 4 Byzantine, the two honest nodes can
    // be shown different quorums. This documents why the validator set must
    // keep faulty weight under one third.
    let ids = identities(4);
    let (a, b) = (block("a"), block("b"));
    let mut left = HeightVote::new(chain(), HEIGHT, set(&ids));
    let mut right = HeightVote::new(chain(), HEIGHT, set(&ids));
    for (hv, honest, value) in [(&mut left, &ids[0], a), (&mut right, &ids[1], b)] {
        hv.on_vote(None, sign_vote(honest, VoteKind::Precommit, 0, Some(value)))
            .unwrap();
        for traitor in &ids[2..4] {
            hv.on_vote(
                None,
                sign_vote(traitor, VoteKind::Precommit, 0, Some(value)),
            )
            .unwrap();
        }
    }
    assert_eq!((left.decided(), right.decided()), (Some(a), Some(b)));
}

#[test]
fn a_decided_block_cannot_be_overturned_in_a_later_round() {
    // The attack locks exist to stop. Three honest validators and one
    // Byzantine (`z`). In round 0 `h0` decides A, but the messages that
    // would tell `h1` and `h2` are delayed. In round 1 a fresh block C is
    // proposed and `z` votes for it. If `h1` (which precommitted A) were
    // free to prevote C, then C would get a polka from h1 + h2 + z, both
    // would precommit it, and with z's precommit C would be decided: two
    // different blocks at one height.
    let ids = identities(4);
    let (h0, h1, h2, z) = (&ids[0], &ids[1], &ids[2], &ids[3]);
    let (a, c) = (block("a"), block("c"));
    let new = || HeightVote::new(chain(), HEIGHT, set(&ids));
    let (mut n0, mut n1, mut n2) = (new(), new(), new());
    let p0 = sign_proposal(proposer(&ids, 0), 0, a, None);

    // Round 0: h0 and h1 see the polka for A and precommit it.
    for (node, id) in [(&mut n0, h0), (&mut n1, h1)] {
        node.on_proposal(Some(id), p0.clone(), true).unwrap();
        for voter in [h0, h1, z] {
            node.on_vote(Some(id), sign_vote(voter, VoteKind::Prevote, 0, Some(a)))
                .unwrap();
        }
        assert_eq!(node.locked(), Some((0, a)));
    }
    // h0 receives the precommits and decides A.
    for voter in [h1, z] {
        n0.on_vote(Some(h0), sign_vote(voter, VoteKind::Precommit, 0, Some(a)))
            .unwrap();
    }
    assert_eq!(n0.decided(), Some(a));
    // h1 and h2 see none of that in time and move to round 1.
    n1.on_timeout(Some(h1), 0, Step::Precommit);
    n2.on_timeout(Some(h2), 0, Step::Propose);
    n2.on_timeout(Some(h2), 0, Step::Prevote);
    n2.on_timeout(Some(h2), 0, Step::Precommit);

    // Round 1: C is proposed; h2 has no lock and prevotes it; z does too.
    let p1 = sign_proposal(proposer(&ids, 1), 1, c, None);
    let from_h1 = broadcasts(&n1.on_proposal(Some(h1), p1.clone(), true).unwrap());
    let from_h2 = broadcasts(&n2.on_proposal(Some(h2), p1, true).unwrap());
    assert_eq!(
        from_h1,
        [sign_vote(h1, VoteKind::Prevote, 1, None)],
        "h1 is locked on A"
    );
    assert_eq!(from_h2, [sign_vote(h2, VoteKind::Prevote, 1, Some(c))]);

    // Deliver every round-1 vote that exists, including z's for C.
    let z_votes = [
        sign_vote(z, VoteKind::Prevote, 1, Some(c)),
        sign_vote(z, VoteKind::Precommit, 1, Some(c)),
    ];
    for node_and_id in [(&mut n1, h1), (&mut n2, h2)] {
        let (node, id) = node_and_id;
        for vote in from_h1.iter().chain(&from_h2).chain(&z_votes) {
            node.on_vote(Some(id), vote.clone()).unwrap();
        }
        assert_eq!(node.decided(), None, "C must not be decided");
        assert_ne!(node.locked().map(|(_, b)| b), Some(c), "nobody locks C");
    }
}
