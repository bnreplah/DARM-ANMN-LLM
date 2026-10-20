use super::*;
use crate::ledger::store::BlockLog;
use crate::ledger::{EntryBody, ProposalOutcome, ValidatorSet};

const NOW: u64 = 1_700_000_000_000;

fn identity(seed: u8) -> NodeIdentity {
    NodeIdentity::from_secret_bytes(&[seed; 32])
}

fn config(validators: &[&NodeIdentity]) -> ChainConfig {
    ChainConfig {
        name: "test-mesh".into(),
        validators: ValidatorSet::equal(validators.iter().map(|v| v.node_id())).unwrap(),
    }
}

fn commit(chain: &Hash32, author: &NodeIdentity, seed: u8) -> SignedEntry {
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

/// Drive one height to finality across `validators`, each holding its own
/// ledger replica. Returns the finalized block.
fn finalize_round(ledgers: &mut [Ledger], validators: &[&NodeIdentity], round: u32) -> Block {
    let proposer_id = ledgers[0].proposer_for_next(round);
    let proposer_index = validators
        .iter()
        .position(|v| v.node_id() == proposer_id)
        .unwrap();
    let block = ledgers[proposer_index]
        .propose(validators[proposer_index], round, NOW)
        .unwrap()
        .expect("pending entries exist");
    let hash = block.hash();
    for (i, ledger) in ledgers.iter_mut().enumerate() {
        if i != proposer_index {
            ledger.receive_proposal(block.clone(), NOW).unwrap();
        }
    }
    let mut attestations = Vec::new();
    for (ledger, validator) in ledgers.iter_mut().zip(validators) {
        let (attestation, _) = ledger.attest(validator, &hash).unwrap();
        attestations.push(attestation);
    }
    for ledger in ledgers.iter_mut() {
        for attestation in &attestations {
            if ledger.candidate(&hash).is_some() {
                ledger
                    .receive_attestation(&hash, attestation.clone())
                    .unwrap();
            }
        }
    }
    ledgers[0].blocks().last().unwrap().clone()
}

#[test]
fn single_validator_finalizes_alone() {
    let node = identity(1);
    let mut ledger = Ledger::new(config(&[&node]));
    let chain = ledger.chain_id();
    let entry = commit(&chain, &node, 1);
    assert!(ledger.submit(entry.clone(), NOW).unwrap());
    assert!(
        !ledger.submit(entry, NOW).unwrap(),
        "resubmission is a no-op"
    );
    let block = ledger.propose(&node, 0, NOW).unwrap().unwrap();
    let (_, finalized) = ledger.attest(&node, &block.hash()).unwrap();
    assert_eq!(finalized, Some(1));
    assert_eq!(ledger.height(), 1);
    assert_eq!(ledger.pending_len(), 0);
    let entry_id = ledger.block(1).unwrap().entries[0].id();
    let (height, proof) = ledger.locate_entry(&entry_id).unwrap();
    assert_eq!(height, 1);
    assert!(merkle::verify(
        &ledger.block(1).unwrap().header.entries_root,
        &entry_id,
        &proof
    ));
}

#[test]
fn three_validators_reach_quorum_and_agree() {
    let validators = [identity(1), identity(2), identity(3)];
    let refs: Vec<&NodeIdentity> = validators.iter().collect();
    let mut ledgers: Vec<Ledger> = (0..3).map(|_| Ledger::new(config(&refs))).collect();
    let chain = ledgers[0].chain_id();
    let entry = commit(&chain, &validators[0], 9);
    for ledger in &mut ledgers {
        ledger.submit(entry.clone(), NOW).unwrap();
    }
    let block = finalize_round(&mut ledgers, &refs, 0);
    for ledger in &ledgers {
        assert_eq!(ledger.height(), 1);
        assert_eq!(ledger.head_hash(), block.hash());
    }
    // A fresh replica can replay the finalized history.
    let replayed = Ledger::replay(config(&refs), ledgers[0].blocks().to_vec()).unwrap();
    assert_eq!(replayed.head_hash(), block.hash());
}

#[test]
fn two_of_three_is_not_final() {
    let validators = [identity(1), identity(2), identity(3)];
    let refs: Vec<&NodeIdentity> = validators.iter().collect();
    let mut ledger = Ledger::new(config(&refs));
    let chain = ledger.chain_id();
    ledger
        .submit(commit(&chain, &validators[0], 1), NOW)
        .unwrap();
    let proposer = refs
        .iter()
        .find(|v| v.node_id() == ledger.proposer_for_next(0))
        .unwrap();
    let block = ledger.propose(proposer, 0, NOW).unwrap().unwrap();
    let hash = block.hash();
    for validator in refs.iter().take(2) {
        let (_, finalized) = ledger.attest(validator, &hash).unwrap();
        assert_eq!(finalized, None);
    }
    assert_eq!(ledger.height(), 0);
}

#[test]
fn rejects_forged_and_cross_chain_entries() {
    let node = identity(1);
    let mut ledger = Ledger::new(config(&[&node]));
    let chain = ledger.chain_id();
    let mut forged = commit(&chain, &node, 1);
    forged.entry.author = identity(2).node_id();
    assert_eq!(ledger.submit(forged, NOW), Err(DarmError::InvalidSignature));

    let other_chain = Hash32::digest("chain-id", b"other");
    let replayed = commit(&other_chain, &node, 1);
    assert!(matches!(
        ledger.submit(replayed, NOW),
        Err(DarmError::Rejected(_))
    ));

    let mut future = commit(&chain, &node, 2);
    future.entry.timestamp_ms = NOW + 60 * 60 * 1_000;
    future.signature = node.sign(&future.entry.canonical());
    assert!(matches!(
        ledger.submit(future, NOW),
        Err(DarmError::Rejected(_))
    ));
}

#[test]
fn validator_never_attests_two_blocks_at_one_height() {
    let node = identity(1);
    let mut ledger = Ledger::with_limits(
        config(&[&node, &identity(2), &identity(3), &identity(4)]),
        LedgerLimits::default(),
    );
    let chain = ledger.chain_id();
    let proposer_id = ledger.proposer_for_next(0);
    let all = [identity(1), identity(2), identity(3), identity(4)];
    let proposer = all.iter().find(|v| v.node_id() == proposer_id).unwrap();
    ledger.submit(commit(&chain, &node, 1), NOW).unwrap();
    let first = ledger.propose(proposer, 0, NOW).unwrap().unwrap();
    // The same proposer equivocates with a second block at the same height.
    ledger.submit(commit(&chain, &node, 2), NOW).unwrap();
    let second = ledger.propose(proposer, 0, NOW + 1).unwrap().unwrap();
    assert_ne!(first.hash(), second.hash());
    ledger.attest(&node, &first.hash()).unwrap();
    assert!(matches!(
        ledger.attest(&node, &second.hash()),
        Err(DarmError::Rejected(_))
    ));
}

#[test]
fn rejects_wrong_proposer_and_tampered_blocks() {
    let a = identity(1);
    let b = identity(2);
    let mut ledger = Ledger::new(config(&[&a, &b]));
    let chain = ledger.chain_id();
    ledger.submit(commit(&chain, &a, 1), NOW).unwrap();
    let designated = ledger.proposer_for_next(0);
    let (right, wrong) = if designated == a.node_id() {
        (&a, &b)
    } else {
        (&b, &a)
    };
    assert!(matches!(
        ledger.propose(wrong, 0, NOW),
        Err(DarmError::Unauthorized(_))
    ));
    let block = ledger.propose(right, 0, NOW).unwrap().unwrap();

    let mut peer = Ledger::new(config(&[&a, &b]));
    let mut tampered = block.clone();
    tampered.entries[0].entry.timestamp_ms += 1;
    assert!(peer.receive_proposal(tampered, NOW).is_err());

    let mut resigned = block.clone();
    resigned.header.round = 1;
    assert!(peer.receive_proposal(resigned, NOW).is_err());

    assert!(peer.receive_proposal(block, NOW).is_ok());
}

#[test]
fn replay_detects_missing_quorum_and_tampering() {
    let node = identity(1);
    let mut ledger = Ledger::new(config(&[&node]));
    let chain = ledger.chain_id();
    ledger.submit(commit(&chain, &node, 1), NOW).unwrap();
    let block = ledger.propose(&node, 0, NOW).unwrap().unwrap();
    ledger.attest(&node, &block.hash()).unwrap();
    let good = ledger.blocks().to_vec();

    let mut stripped = good.clone();
    stripped[0].attestations.clear();
    assert!(Ledger::replay(config(&[&node]), stripped).is_err());

    let mut outsider = good.clone();
    outsider[0].attestations = vec![Attestation {
        validator: identity(9).node_id(),
        signature: identity(9).sign(&attestation_message(&chain, 1, &good[0].hash())),
    }];
    assert!(Ledger::replay(config(&[&node]), outsider).is_err());

    assert!(Ledger::replay(config(&[&node]), good).is_ok());
}

#[test]
fn state_rules_revocation_reputation_and_votes() {
    let a = identity(1);
    let b = identity(2);
    let mut ledger = Ledger::new(config(&[&a]));
    let chain = ledger.chain_id();
    let memory = Hash32::digest("memory", b"m");
    let sign = |who: &NodeIdentity, body| SignedEntry::sign(who, chain, body, NOW).unwrap();
    ledger
        .submit(
            sign(
                &a,
                EntryBody::MemoryCommit {
                    memory,
                    importance_milli: 900,
                },
            ),
            NOW,
        )
        .unwrap();
    // b may not revoke a's memory; the proposer drops the invalid entry.
    ledger
        .submit(sign(&b, EntryBody::MemoryRevoke { memory }), NOW)
        .unwrap();
    // Non-validators cannot move reputation.
    ledger
        .submit(
            sign(
                &b,
                EntryBody::ReputationReport {
                    subject: a.node_id(),
                    delta_milli: -1_000,
                    evidence: Hash32::ZERO,
                },
            ),
            NOW,
        )
        .unwrap();
    let proposal = sign(
        &a,
        EntryBody::Proposal {
            kind: "replicate".into(),
            payload: Hash32::digest("doc", b"spawn worker"),
            deadline_ms: NOW + 60_000,
        },
    );
    let proposal_id = proposal.id();
    ledger.submit(proposal, NOW).unwrap();
    let block = ledger.propose(&a, 0, NOW).unwrap().unwrap();
    assert_eq!(block.entries.len(), 2, "invalid entries are dropped");
    ledger.attest(&a, &block.hash()).unwrap();
    assert!(ledger.state().is_committed(&memory));
    assert_eq!(ledger.state().reputation(&a.node_id()), 1_000);

    ledger
        .submit(
            sign(
                &a,
                EntryBody::Vote {
                    proposal: proposal_id,
                    support: true,
                },
            ),
            NOW,
        )
        .unwrap();
    let block = ledger.propose(&a, 0, NOW).unwrap().unwrap();
    ledger.attest(&a, &block.hash()).unwrap();
    let outcome = ledger
        .state()
        .proposal(&proposal_id)
        .unwrap()
        .outcome(&ledger.config().validators, NOW);
    assert_eq!(outcome, ProposalOutcome::Passed);

    // Self-reports are rejected before they are even signed.
    assert!(
        SignedEntry::sign(
            &a,
            chain,
            EntryBody::ReputationReport {
                subject: a.node_id(),
                delta_milli: 1,
                evidence: Hash32::ZERO
            },
            NOW
        )
        .is_err()
    );
}

#[test]
fn pending_pool_is_bounded_per_author() {
    let node = identity(1);
    let limits = LedgerLimits {
        max_pending_per_author: 2,
        ..LedgerLimits::default()
    };
    let mut ledger = Ledger::with_limits(config(&[&node]), limits);
    let chain = ledger.chain_id();
    ledger.submit(commit(&chain, &node, 1), NOW).unwrap();
    ledger.submit(commit(&chain, &node, 2), NOW).unwrap();
    assert!(matches!(
        ledger.submit(commit(&chain, &node, 3), NOW),
        Err(DarmError::LimitExceeded(_))
    ));
}

#[test]
fn block_log_round_trip_and_tamper_detection() {
    let dir = tempfile::tempdir().unwrap();
    let log = BlockLog::new(dir.path().join("ledger.jsonl"));
    let node = identity(1);
    let mut ledger = log.load(config(&[&node])).unwrap();
    let chain = ledger.chain_id();
    for seed in 0..3 {
        ledger.submit(commit(&chain, &node, seed), NOW).unwrap();
        let block = ledger.propose(&node, 0, NOW).unwrap().unwrap();
        ledger.attest(&node, &block.hash()).unwrap();
        log.append(ledger.blocks().last().unwrap()).unwrap();
    }
    let reloaded = log.load(config(&[&node])).unwrap();
    assert_eq!(reloaded.head_hash(), ledger.head_hash());

    let text = std::fs::read_to_string(log.path()).unwrap();
    let tampered = text.replacen("\"importance_milli\":500", "\"importance_milli\":999", 1);
    std::fs::write(log.path(), tampered).unwrap();
    assert!(log.load(config(&[&node])).is_err());
}

#[test]
fn compute_commits_finalize_once_per_job() {
    let a = identity(1);
    let b = identity(2);
    let mut ledger = Ledger::new(config(&[&a]));
    let chain = ledger.chain_id();
    let job = Hash32::digest("compute-job", b"embed corpus");
    let body = |seed: &[u8]| EntryBody::ComputeCommit {
        job,
        result_root: Hash32::digest("result", seed),
        receipts_root: Hash32::digest("receipts", seed),
        shards: 8,
        workers: 3,
    };
    ledger
        .submit(SignedEntry::sign(&a, chain, body(b"a"), NOW).unwrap(), NOW)
        .unwrap();
    // A second commit by the same coordinator is a duplicate and dropped.
    ledger
        .submit(SignedEntry::sign(&a, chain, body(b"a2"), NOW).unwrap(), NOW)
        .unwrap();
    // Someone copying the job id commits only under their own name and
    // cannot displace the coordinator's record.
    ledger
        .submit(SignedEntry::sign(&b, chain, body(b"b"), NOW).unwrap(), NOW)
        .unwrap();
    let block = ledger.propose(&a, 0, NOW).unwrap().unwrap();
    assert_eq!(block.entries.len(), 2);
    ledger.attest(&a, &block.hash()).unwrap();
    let record = ledger.state().compute_job(&a.node_id(), &job).unwrap();
    assert_eq!(record.coordinator, a.node_id());
    assert!(
        [
            Hash32::digest("result", b"a"),
            Hash32::digest("result", b"a2")
        ]
        .contains(&record.result_root)
    );
    assert_eq!(
        ledger
            .state()
            .compute_job(&b.node_id(), &job)
            .unwrap()
            .result_root,
        Hash32::digest("result", b"b")
    );
    assert_eq!(ledger.state().compute_jobs(), 2);

    for (shards, workers) in [(0, 1), (crate::ledger::MAX_COMPUTE_SHARDS + 1, 1), (1, 0)] {
        let bad = EntryBody::ComputeCommit {
            job,
            result_root: Hash32::ZERO,
            receipts_root: Hash32::ZERO,
            shards,
            workers,
        };
        assert!(SignedEntry::sign(&a, chain, bad, NOW).is_err());
    }
}
