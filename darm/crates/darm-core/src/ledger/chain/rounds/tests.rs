use super::*;
use crate::Hash32;
use crate::ledger::locking::{VoteKind, vote_message};
use crate::ledger::store::{BlockLog, VoteLog};
use crate::ledger::{ChainConfig, EntryBody, SignedEntry, ValidatorSet};
use std::collections::VecDeque;

const NOW: u64 = 1_700_000_000_000;

fn identities(n: u8) -> Vec<NodeIdentity> {
    (1..=n)
        .map(|seed| NodeIdentity::from_secret_bytes(&[seed; 32]))
        .collect()
}

fn config(ids: &[NodeIdentity]) -> ChainConfig {
    ChainConfig {
        name: "locking-test".into(),
        validators: ValidatorSet::equal(ids.iter().map(NodeIdentity::node_id)).unwrap(),
    }
}

fn entry(chain: &Hash32, author: &NodeIdentity, seed: u8) -> SignedEntry {
    SignedEntry::sign(
        author,
        *chain,
        EntryBody::MemoryCommit {
            memory: Hash32::digest("memory", &[seed]),
            importance_milli: 500,
        },
        NOW,
    )
    .unwrap()
}

/// One ledger replica per validator, all in locking mode.
struct Net {
    ids: Vec<NodeIdentity>,
    ledgers: Vec<Ledger>,
    /// Validators whose messages are dropped (crashed or Byzantine-silent).
    silent: Vec<bool>,
}

impl Net {
    fn new(n: u8) -> Self {
        let ids = identities(n);
        let ledgers = ids
            .iter()
            .map(|_| {
                let mut ledger = Ledger::new(config(&ids));
                ledger.enable_locking();
                ledger
            })
            .collect();
        Self {
            silent: vec![false; ids.len()],
            ids,
            ledgers,
        }
    }

    fn index_of(&self, node: crate::NodeId) -> usize {
        self.ids.iter().position(|id| id.node_id() == node).unwrap()
    }

    fn proposer(&self, round: u32) -> usize {
        self.index_of(self.ledgers[0].locking().unwrap().proposer(round))
    }

    fn submit_everywhere(&mut self, seed: u8) {
        let chain = self.ledgers[0].chain_id();
        let signed = entry(&chain, &self.ids[0], seed);
        for ledger in &mut self.ledgers {
            ledger.submit(signed.clone(), NOW).unwrap();
        }
    }

    /// Deliver the votes in `actions` (produced by `from`) to everyone
    /// else, and keep going until the network is quiet.
    fn flood(&mut self, from: usize, actions: Vec<Action>) {
        let mut queue: VecDeque<(usize, Vote)> = VecDeque::new();
        let push = |queue: &mut VecDeque<(usize, Vote)>, from: usize, actions: Vec<Action>| {
            for action in actions {
                if let Action::Broadcast(vote) = action {
                    queue.push_back((from, vote));
                }
            }
        };
        push(&mut queue, from, actions);
        while let Some((sender, vote)) = queue.pop_front() {
            if self.silent[sender] {
                continue;
            }
            for to in 0..self.ids.len() {
                if to == sender || self.silent[to] {
                    continue;
                }
                // A node that already finalized this height rejects late
                // votes for it; that is expected.
                if let Ok(actions) =
                    self.ledgers[to].on_round_vote(Some(&self.ids[to]), vote.clone())
                {
                    push(&mut queue, to, actions);
                }
            }
        }
    }

    fn send_proposal(&mut self, from: usize, message: &RoundProposal, to: &[usize]) {
        for &target in to {
            if target == from || self.silent[target] {
                continue;
            }
            let actions = self.ledgers[target]
                .on_round_proposal(Some(&self.ids[target]), message.clone(), NOW)
                .unwrap();
            self.flood(target, actions);
        }
    }

    /// The current round's proposer proposes to everyone.
    fn propose(&mut self) -> Option<RoundProposal> {
        let round = self
            .ledgers
            .iter()
            .map(|l| l.locking().unwrap().round())
            .max()
            .unwrap();
        let proposer = self.proposer(round);
        let id = NodeIdentity::from_secret_bytes(&[proposer as u8 + 1; 32]);
        let (message, actions) = self.ledgers[proposer].propose_round(&id, NOW).unwrap()?;
        self.flood(proposer, actions);
        let everyone: Vec<usize> = (0..self.ids.len()).collect();
        self.send_proposal(proposer, &message, &everyone);
        Some(message)
    }

    /// Fire every live node's current timer once.
    fn timeouts(&mut self) {
        for index in 0..self.ids.len() {
            if self.silent[index] {
                continue;
            }
            let (round, step) = {
                let rounds = self.ledgers[index].locking().unwrap();
                (rounds.round(), rounds.step())
            };
            let actions = self.ledgers[index]
                .on_round_timeout(Some(&self.ids[index]), round, step)
                .unwrap();
            self.flood(index, actions);
        }
    }

    fn heights(&self) -> Vec<u64> {
        self.ledgers.iter().map(Ledger::height).collect()
    }
}

#[test]
fn a_block_is_finalized_with_a_precommit_certificate() {
    let mut net = Net::new(4);
    net.submit_everywhere(1);
    let message = net.propose().expect("the proposer has an entry");
    assert_eq!(net.heights(), [1, 1, 1, 1]);
    let block = net.ledgers[0].block(1).unwrap().clone();
    assert_eq!(block.hash(), message.block.hash());
    assert_eq!(block.commit_round, Some(0));
    for ledger in &net.ledgers {
        assert_eq!(ledger.head_hash(), block.hash());
        // The next height starts clean.
        let rounds = ledger.locking().unwrap();
        assert_eq!(
            (rounds.height(), rounds.round(), rounds.locked()),
            (2, 0, None)
        );
    }

    // The certificate verifies on its own: replay from scratch, through
    // JSON, and through the on-disk log.
    Ledger::replay(config(&net.ids), vec![block.clone()]).unwrap();
    let json = serde_json::to_string(&block).unwrap();
    assert!(json.contains("\"commit_round\":0"));
    let parsed: Block = serde_json::from_str(&json).unwrap();
    assert_eq!(parsed, block);
    let dir = std::env::temp_dir().join(format!("darm-rounds-{}", std::process::id()));
    let log = BlockLog::new(dir.join("ledger.jsonl"));
    let _ = std::fs::remove_dir_all(&dir);
    log.append(&block).unwrap();
    assert_eq!(log.load(config(&net.ids)).unwrap().height(), 1);
    std::fs::remove_dir_all(&dir).unwrap();

    // A second height works the same way.
    net.submit_everywhere(2);
    net.propose().unwrap();
    assert_eq!(net.heights(), [2, 2, 2, 2]);
}

#[test]
fn an_offline_proposer_costs_one_round() {
    let mut net = Net::new(4);
    net.submit_everywhere(1);
    let absent = net.proposer(0);
    net.silent[absent] = true;
    // The propose timer fires with nothing to vote on: everyone prevotes
    // nil, the nil polka makes them precommit nil, and the precommit timer
    // moves them to round 1.
    net.timeouts();
    net.timeouts();
    assert!(
        net.ledgers
            .iter()
            .enumerate()
            .all(|(i, l)| i == absent || l.locking().unwrap().round() == 1)
    );
    net.propose().expect("the round-1 proposer is live");
    for (index, ledger) in net.ledgers.iter().enumerate() {
        if index != absent {
            assert_eq!(ledger.height(), 1);
            assert_eq!(ledger.block(1).unwrap().commit_round, Some(1));
        }
    }
}

#[test]
fn an_equivocating_proposer_cannot_stall_the_ledger() {
    let mut net = Net::new(4);
    let chain = net.ledgers[0].chain_id();
    let byzantine = net.proposer(0);
    net.silent[byzantine] = true;
    let honest: Vec<usize> = (0..4).filter(|i| *i != byzantine).collect();

    // The proposer builds two different blocks for round 0 by keeping two
    // views of the pending pool, and signs a proposal for each.
    let id = NodeIdentity::from_secret_bytes(&[byzantine as u8 + 1; 32]);
    let make = |seed: u8| {
        let mut ledger = Ledger::new(config(&net.ids));
        ledger.enable_locking();
        ledger.submit(entry(&chain, &id, seed), NOW).unwrap();
        ledger.propose_round(&id, NOW).unwrap().unwrap().0
    };
    let (a, b) = (make(10), make(11));
    assert_ne!(a.block.hash(), b.block.hash());

    // Honest validators also hold a real entry to propose later.
    for &index in &honest {
        net.ledgers[index]
            .submit(entry(&chain, &net.ids[honest[0]], 1), NOW)
            .unwrap();
    }
    net.send_proposal(byzantine, &a, &honest[..1]);
    net.send_proposal(byzantine, &b, &honest[1..]);
    assert!(
        honest.iter().all(|&i| net.ledgers[i].height() == 0),
        "the vote split"
    );
    // A validator shown the other block later refuses it for this round.
    assert!(
        net.ledgers[honest[0]]
            .on_round_proposal(Some(&net.ids[honest[0]]), b.clone(), NOW)
            .is_err()
    );

    // Under the one-vote rule this height is now dead. Here the round
    // times out and the next proposer finishes it.
    net.timeouts();
    net.timeouts();
    let mut rounds = 0;
    while honest.iter().any(|&i| net.ledgers[i].height() == 0) {
        rounds += 1;
        assert!(rounds < 8, "the height did not finish");
        if net.propose().is_none() {
            net.timeouts();
            net.timeouts();
            net.timeouts();
        }
    }
    let head = net.ledgers[honest[0]].head_hash();
    assert!(honest.iter().all(|&i| net.ledgers[i].head_hash() == head));
    assert!(net.ledgers[honest[0]].block(1).unwrap().commit_round >= Some(1));
}

#[test]
fn certificates_of_the_wrong_kind_or_round_are_rejected() {
    let mut net = Net::new(4);
    net.submit_everywhere(1);
    net.propose().unwrap();
    let good = net.ledgers[0].block(1).unwrap().clone();
    let verify = |block: Block| Ledger::replay(config(&net.ids), vec![block]);
    verify(good.clone()).unwrap();

    // Precommits presented as one-vote attestations, and the reverse.
    let mut stripped = good.clone();
    stripped.commit_round = None;
    assert_eq!(verify(stripped).unwrap_err(), DarmError::InvalidSignature);
    // Precommits from round 0 presented as round 1.
    let mut moved = good.clone();
    moved.commit_round = Some(1);
    assert_eq!(verify(moved).unwrap_err(), DarmError::InvalidSignature);
    // Too few signers.
    let mut thin = good.clone();
    thin.attestations.truncate(2);
    assert!(matches!(verify(thin), Err(DarmError::Rejected(_))));
    // Padding with repeats does not make a quorum.
    let mut padded = good.clone();
    padded.attestations.truncate(2);
    padded.attestations.push(padded.attestations[0].clone());
    assert!(matches!(verify(padded), Err(DarmError::Rejected(_))));
    // More signatures than validators is refused outright.
    let mut bloated = good.clone();
    bloated.attestations = vec![good.attestations[0].clone(); 5];
    assert!(matches!(verify(bloated), Err(DarmError::LimitExceeded(_))));
    // Prevotes are not precommits.
    let mut prevotes = good.clone();
    let hash = good.hash();
    let chain = net.ledgers[0].chain_id();
    prevotes.attestations = net
        .ids
        .iter()
        .map(|id| Attestation {
            validator: id.node_id(),
            signature: id.sign(&vote_message(&chain, VoteKind::Prevote, 1, 0, Some(&hash))),
        })
        .collect();
    assert_eq!(verify(prevotes).unwrap_err(), DarmError::InvalidSignature);
}

#[test]
fn one_vote_blocks_still_verify_and_the_two_rules_do_not_mix() {
    let ids = identities(4);
    // Finalize a block under the one-vote rule.
    let mut legacy: Vec<Ledger> = ids.iter().map(|_| Ledger::new(config(&ids))).collect();
    let chain = legacy[0].chain_id();
    let signed = entry(&chain, &ids[0], 1);
    for ledger in &mut legacy {
        ledger.submit(signed.clone(), NOW).unwrap();
    }
    let proposer = ids
        .iter()
        .position(|id| id.node_id() == legacy[0].proposer_for_next(0))
        .unwrap();
    let block = legacy[proposer]
        .propose(&ids[proposer], 0, NOW)
        .unwrap()
        .unwrap();
    let hash = block.hash();
    let mut finalized = None;
    for id in &ids {
        if legacy[proposer].attest(id, &hash).unwrap().1.is_some() {
            finalized = legacy[proposer].block(1).cloned();
            break;
        }
    }
    let finalized = finalized.expect("quorum reached");
    assert_eq!(finalized.commit_round, None);
    assert!(
        !serde_json::to_string(&finalized)
            .unwrap()
            .contains("commit_round")
    );

    // A locking ledger accepts that history and continues from it...
    let mut ledger = Ledger::replay(config(&ids), vec![finalized]).unwrap();
    ledger.enable_locking();
    assert_eq!(ledger.locking().unwrap().height(), 2);
    // ...but will not sign or count one-vote attestations any more.
    assert!(ledger.attest(&ids[0], &hash).is_err());
    // And a ledger that has not enabled locking has no round machine.
    assert!(legacy[0].propose_round(&ids[0], NOW).is_err());
}

#[test]
fn a_restarted_validator_keeps_its_lock_and_never_signs_twice() {
    let ids = identities(4);
    let dir = std::env::temp_dir().join(format!("darm-votes-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let log = VoteLog::new(dir.join("votes.jsonl"));

    // Build a round-0 proposal with a throwaway proposer ledger.
    let chain = Ledger::new(config(&ids)).chain_id();
    let proposer_node = ValidatorSet::equal(ids.iter().map(NodeIdentity::node_id))
        .unwrap()
        .proposer_for(1, 0);
    let proposer = ids
        .iter()
        .position(|id| id.node_id() == proposer_node)
        .unwrap();
    // The validator under test is not the round-0 proposer.
    let me = (proposer + 1) % ids.len();
    let mut source = Ledger::new(config(&ids));
    source.enable_locking();
    source
        .submit(entry(&chain, &ids[proposer], 1), NOW)
        .unwrap();
    let (message, _) = source.propose_round(&ids[proposer], NOW).unwrap().unwrap();
    let a = message.block.hash();

    // This validator prevotes A, sees the polka, locks and precommits,
    // persisting each vote before it would be sent.
    let mut ledger = Ledger::new(config(&ids));
    ledger.enable_locking();
    let mut sent = Vec::new();
    let record = |actions: Vec<Action>, sent: &mut Vec<Vote>| {
        for action in actions {
            if let Action::Broadcast(vote) = action {
                log.append(&vote).unwrap();
                sent.push(vote);
            }
        }
    };
    let actions = ledger
        .on_round_proposal(Some(&ids[me]), message.clone(), NOW)
        .unwrap();
    record(actions, &mut sent);
    for other in ids
        .iter()
        .enumerate()
        .filter(|(i, _)| *i != me)
        .map(|(_, id)| id)
        .take(2)
    {
        let prevote = Vote {
            kind: VoteKind::Prevote,
            height: 1,
            round: 0,
            block: Some(a),
            validator: other.node_id(),
            signature: other.sign(&vote_message(&chain, VoteKind::Prevote, 1, 0, Some(&a))),
        };
        let actions = ledger.on_round_vote(Some(&ids[me]), prevote).unwrap();
        record(actions, &mut sent);
    }
    assert_eq!(ledger.locking().unwrap().locked(), Some((0, a)));
    assert_eq!(sent.len(), 2, "one prevote and one precommit");

    // Crash. Everything in memory is gone; the vote log is not.
    drop(ledger);
    let mut ledger = Ledger::new(config(&ids));
    ledger.enable_locking();
    for vote in log.load(1).unwrap() {
        ledger.restore_round_vote(vote).unwrap();
    }
    let rounds = ledger.locking().unwrap();
    assert_eq!(rounds.locked(), Some((0, a)), "the lock survived");
    assert_eq!(rounds.step(), Step::Precommit);

    // Seeing the same proposal again produces no second signature.
    let actions = ledger
        .on_round_proposal(Some(&ids[me]), message, NOW)
        .unwrap();
    assert!(actions.iter().all(|a| !matches!(a, Action::Broadcast(_))));
    // In round 1 a different block gets nil from this validator.
    ledger
        .on_round_timeout(Some(&ids[me]), 0, Step::Precommit)
        .unwrap();
    assert_eq!(ledger.locking().unwrap().round(), 1);
    let next = ids
        .iter()
        .position(|id| id.node_id() == ledger.locking().unwrap().proposer(1))
        .unwrap();
    if next != me {
        let mut other = Ledger::new(config(&ids));
        other.enable_locking();
        other.submit(entry(&chain, &ids[next], 9), NOW).unwrap();
        other.on_round_timeout(None, 0, Step::Propose).unwrap();
        other.on_round_timeout(None, 0, Step::Prevote).unwrap();
        other.on_round_timeout(None, 0, Step::Precommit).unwrap();
        let (c, _) = other.propose_round(&ids[next], NOW).unwrap().unwrap();
        assert_ne!(c.block.hash(), a);
        let actions = ledger.on_round_proposal(Some(&ids[me]), c, NOW).unwrap();
        let votes: Vec<_> = actions
            .iter()
            .filter_map(|a| match a {
                Action::Broadcast(v) => Some((v.kind, v.round, v.block)),
                _ => None,
            })
            .collect();
        assert_eq!(votes, [(VoteKind::Prevote, 1, None)]);
    }

    // The log refuses to guess past damage, and clears once final.
    std::fs::write(dir.join("votes.jsonl"), b"{not json\n").unwrap();
    assert!(log.load(1).is_err());
    log.clear().unwrap();
    assert!(log.load(1).unwrap().is_empty());
    log.clear().unwrap();
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn bad_proposals_are_refused_or_answered_with_nil() {
    let mut net = Net::new(4);
    net.submit_everywhere(1);
    let proposer = net.proposer(0);
    let id = NodeIdentity::from_secret_bytes(&[proposer as u8 + 1; 32]);
    let (message, _) = net.ledgers[proposer]
        .propose_round(&id, NOW)
        .unwrap()
        .unwrap();
    // The proposer does not propose twice in one round.
    assert!(
        net.ledgers[proposer]
            .propose_round(&id, NOW)
            .unwrap()
            .is_none()
    );
    let target = (0..4).find(|i| *i != proposer).unwrap();
    let me = NodeIdentity::from_secret_bytes(&[target as u8 + 1; 32]);

    // The block must be the one the proposal names.
    let mut swapped = message.clone();
    swapped.block.header.timestamp_ms += 1;
    assert!(
        net.ledgers[target]
            .on_round_proposal(Some(&me), swapped, NOW)
            .is_err()
    );
    // A proposal signed by someone who is not the round's proposer is
    // refused before its block is looked at.
    let mut forged = message.clone();
    forged.proposal.proposer = me.node_id();
    assert!(
        net.ledgers[target]
            .on_round_proposal(Some(&me), forged, NOW)
            .is_err()
    );
    assert!(
        net.ledgers[target]
            .candidate(&message.block.hash())
            .is_none()
    );
    // A certificate smuggled in with a proposal is discarded.
    let mut preloaded = message.clone();
    preloaded.block.commit_round = Some(0);
    preloaded.block.attestations = vec![Attestation {
        validator: id.node_id(),
        signature: message.block.proposer_signature,
    }];
    net.ledgers[target]
        .on_round_proposal(Some(&me), preloaded, NOW)
        .unwrap();
    let held = net.ledgers[target]
        .candidate(&message.block.hash())
        .unwrap();
    assert!(held.attestations.is_empty() && held.commit_round.is_none());
    assert_eq!(net.ledgers[target].height(), 0);

    // A block from too far in the future is answered with a nil prevote.
    let mut late = Net::new(4);
    late.submit_everywhere(1);
    let (message, _) = late.ledgers[proposer]
        .propose_round(&id, NOW)
        .unwrap()
        .unwrap();
    let actions = late.ledgers[target]
        .on_round_proposal(Some(&me), message, NOW - 10 * 60 * 1_000)
        .unwrap();
    assert!(matches!(
        actions.as_slice(),
        [Action::Broadcast(Vote {
            kind: VoteKind::Prevote,
            block: None,
            ..
        })]
    ));
}
