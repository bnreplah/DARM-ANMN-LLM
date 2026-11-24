//! Deterministic protocol test vectors. Another implementation of DARM-ANN
//! must reproduce every hex value here from the listed inputs.

use darm_core::canonical::Canonical;
use darm_core::ledger::locking::{Vote, VoteKind, proposal_message, vote_message};
use darm_core::ledger::{
    Attestation, Block, BlockHeader, ChainConfig, EntryBody, Feature, LedgerEntry, Member, Payment,
    SignedEntry, ValidatorSet, attestation_message, block_proposal_message, credit_grant_payload,
    pq_bind_message, validator_set_payload,
};
use darm_core::{Hash32, NodeIdentity, merkle};
use darm_memory::{MemoryKind, MemoryRecord, Scope, SignedMemory};
use serde_json::{Value, json};

pub fn generate() -> darm_core::Result<Value> {
    let identity = NodeIdentity::from_secret_bytes(&[7u8; 32]);
    let node = identity.node_id();
    let chain = ChainConfig::new("darm-test-vectors", ValidatorSet::equal([node])?);
    let chain_id = chain.chain_id();

    let mut canonical = Canonical::new("example");
    canonical.u16(258).str("hi").bool(true).opt_fixed32(None);

    let record = MemoryRecord {
        author: node,
        created_ms: 1_700_000_000_000,
        kind: MemoryKind::Semantic,
        scope: Scope::Mesh,
        importance_milli: 750,
        valence_milli: -125,
        tags: vec!["ns:default".into(), "vectors".into()],
        content: "DARM-ANN test vector memory".into(),
        parent: None,
    };
    let memory = SignedMemory::sign(&identity, record)?;

    // Fixed nonce so the entry id is reproducible (SignedEntry::sign draws a
    // random nonce).
    let entry = LedgerEntry {
        chain: chain_id,
        author: node,
        timestamp_ms: 1_700_000_000_000,
        nonce: 42,
        body: EntryBody::MemoryCommit {
            memory: memory.id(),
            importance_milli: 750,
        },
    };
    let signed_entry = SignedEntry {
        signature: identity.sign(&entry.canonical()),
        entry,
    };
    let ids = vec![signed_entry.id()];
    let header = BlockHeader {
        chain: chain_id,
        height: 1,
        round: 0,
        prev: chain.genesis_hash(),
        entries_root: merkle::root(&ids),
        entry_count: 1,
        timestamp_ms: 1_700_000_000_500,
        proposer: node,
        validators: chain.validators.commitment(),
    };
    let block_hash = header.hash();
    let block = Block {
        proposer_signature: identity.sign(&block_proposal_message(&chain_id, &block_hash)),
        attestations: vec![Attestation {
            validator: node,
            signature: identity.sign(&attestation_message(&chain_id, 1, &block_hash)),
        }],
        header,
        entries: vec![signed_entry.clone()],
        commit_round: None,
    };
    let three: Vec<Hash32> = (0u8..3)
        .map(|i| Hash32::digest("vector-leaf", &[i]))
        .collect();

    // Lock-based finality (spec 4.6): the same block committed in round 3.
    let prevote = vote_message(&chain_id, VoteKind::Prevote, 1, 2, Some(&block_hash));
    let precommit_nil = vote_message(&chain_id, VoteKind::Precommit, 1, 2, None);
    let precommit = vote_message(&chain_id, VoteKind::Precommit, 1, 3, Some(&block_hash));
    let fresh_proposal = proposal_message(&chain_id, 1, 0, &block_hash, None);
    let reproposal = proposal_message(&chain_id, 1, 3, &block_hash, Some(1));
    let locking_block = Block {
        attestations: vec![Attestation {
            validator: node,
            signature: identity.sign(&precommit),
        }],
        commit_round: Some(3),
        ..block.clone()
    };

    // Closed-loop credits (spec 4.7): fixed nonces so ids are reproducible.
    let fixed = |nonce: u64, body: EntryBody| {
        let entry = LedgerEntry {
            chain: chain_id,
            author: node,
            timestamp_ms: 1_700_000_000_000,
            nonce,
            body,
        };
        SignedEntry {
            signature: identity.sign(&entry.canonical()),
            entry,
        }
    };
    let worker = NodeIdentity::from_secret_bytes(&[8u8; 32]).node_id();
    let grant_proposal = Hash32::digest("vector-proposal", b"grant");
    let grant = fixed(
        43,
        EntryBody::CreditGrant {
            proposal: grant_proposal,
            to: node,
            amount: 100,
        },
    );
    let settle = fixed(
        44,
        EntryBody::ComputeSettle {
            job: Hash32::digest("vector-job", b"settle"),
            receipts_root: Hash32::digest("vector-receipts", b"settle"),
            payments: vec![Payment { worker, amount: 30 }],
        },
    );

    // Equivocation evidence (spec 4.9): two prevotes by the vector identity.
    let double_vote = |block: Hash32| {
        let message = vote_message(&chain_id, VoteKind::Prevote, 5, 0, Some(&block));
        Vote {
            kind: VoteKind::Prevote,
            height: 5,
            round: 0,
            block: Some(block),
            validator: node,
            signature: identity.sign(&message),
        }
    };
    let (mut v1, mut v2) = (
        double_vote(Hash32::digest("vector-block", b"a")),
        double_vote(Hash32::digest("vector-block", b"b")),
    );
    if v2.block < v1.block {
        std::mem::swap(&mut v1, &mut v2);
    }
    let equivocation = fixed(
        45,
        EntryBody::Equivocation {
            first: v1,
            second: v2,
        },
    );
    // Validator-set governance (spec 4.10).
    let mut members = vec![
        Member { node, weight: 2 },
        Member {
            node: worker,
            weight: 1,
        },
    ];
    members.sort_by_key(|m| m.node);
    let set_change = fixed(
        46,
        EntryBody::ValidatorSetChange {
            proposal: Hash32::digest("vector-proposal", b"validators"),
            members: members.clone(),
            effective_height: 100,
        },
    );
    // Post-quantum identity (spec 4.11). ML-DSA signing is deterministic,
    // so these values are reproducible.
    let pq = identity.pq_identity();
    let pq_proof = pq.sign("pq-key-bind", &pq_bind_message(&chain_id, &node));
    let pq_bind = fixed(
        47,
        EntryBody::PqKeyBind {
            key: pq.public().as_bytes().to_vec(),
            proof: pq_proof,
        },
    );
    let mut scheduled = chain.clone();
    scheduled.activations = [(Feature::Credits, 1200), (Feature::Evidence, 1500)].into();

    Ok(json!({
        "companion": crate::companion_vectors::generate(),
        "secret_key_seed": "07 repeated 32 times",
        "node_id": node,
        "canonical_example": {
            "domain": "example",
            "fields": "u16 258, str \"hi\", bool true, opt_fixed32 none",
            "bytes": hex(Canonical::finish(canonical)),
        },
        "hash_digest_abc": Hash32::digest("example", b"abc"),
        "chain": {
            "name": chain.name,
            "chain_id": chain_id,
            "validator_set_commitment": chain.validators.commitment(),
            "genesis_hash": chain.genesis_hash(),
        },
        "memory": {
            "record": memory.record,
            "canonical": hex(memory.record.canonical()),
            "id": memory.id(),
            "signature": memory.signature,
        },
        "ledger_entry": {
            "entry": signed_entry,
            "canonical": hex(signed_entry.entry.canonical()),
            "id": signed_entry.id(),
        },
        "block": {
            "block": block,
            "hash": block_hash,
        },
        "lock_based_finality": {
            "prevote_height_1_round_2": hex(prevote),
            "precommit_nil_height_1_round_2": hex(precommit_nil),
            "precommit_height_1_round_3": hex(precommit),
            "proposal_round_0_fresh": hex(fresh_proposal),
            "proposal_round_3_valid_round_1": hex(reproposal),
            "block_committed_in_round_3": locking_block,
        },
        "credits": {
            "grant_payload_for_node_amount_100": credit_grant_payload(&node, 100),
            "credit_grant": {
                "entry": grant,
                "canonical": hex(grant.entry.canonical()),
                "id": grant.id(),
            },
            "compute_settle": {
                "entry": settle,
                "canonical": hex(settle.entry.canonical()),
                "id": settle.id(),
            },
        },
        "equivocation": {
            "entry": equivocation,
            "canonical": hex(equivocation.entry.canonical()),
            "id": equivocation.id(),
        },
        "validator_set_change": {
            "entry": set_change,
            "canonical": hex(set_change.entry.canonical()),
            "id": set_change.id(),
            "payload_for_members_effective_100": validator_set_payload(&members, 100),
        },
        "pq_key_bind": {
            "ml_dsa_seed": Hash32::digest("pq-identity-seed", &[7u8; 32]),
            "public_key_fingerprint": pq.public().fingerprint(),
            "bind_message": hex(pq_bind_message(&chain_id, &node)),
            "entry": pq_bind,
            "canonical_hash": Hash32::digest("vector-canonical", &pq_bind.entry.canonical()),
            "id": pq_bind.id(),
        },
        "activations": {
            "empty_commitment": chain.activations_commitment(),
            "credits_1200_evidence_1500_commitment": scheduled.activations_commitment(),
        },
        "hybrid_sessions": {
            "psk_from_shared_secret_0x11_x32": Hash32::digest("session-hybrid-psk", &[0x11u8; 32]),
            "suite_codes": {"noise_xx_cha_cha_poly": 1, "noise_xx_aes_gcm": 2, "hybrid_ml_kem768_cha_cha_poly": 3, "hybrid_ml_kem768_aes_gcm": 4},
            "prologue_labels": {
                "hybrid_ml_kem768_cha_cha_poly": "Noise_XXpsk3_25519+MLKEM768_ChaChaPoly_BLAKE2s",
                "hybrid_ml_kem768_aes_gcm": "Noise_XXpsk3_25519+MLKEM768_AESGCM_SHA256",
            },
        },
        "merkle": {
            "leaves": three,
            "root": merkle::root(&three),
            "proof_index_2": merkle::prove(&three, 2),
            "empty_root": merkle::root(&[]),
        },
    }))
}

fn hex(bytes: Vec<u8>) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use darm_core::ledger::Ledger;

    #[test]
    fn vectors_are_deterministic_and_self_consistent() {
        let a = generate().unwrap();
        assert_eq!(a, generate().unwrap());
        // The vector block must be accepted by a real ledger.
        let block: Block = serde_json::from_value(a["block"]["block"].clone()).unwrap();
        let identity = NodeIdentity::from_secret_bytes(&[7u8; 32]);
        let config = ChainConfig::new(
            "darm-test-vectors",
            ValidatorSet::equal([identity.node_id()]).unwrap(),
        );
        Ledger::replay(config.clone(), vec![block]).unwrap();
        // So must the same block with a round-3 precommit certificate, and
        // not with that certificate claimed for another round.
        let locking: Block =
            serde_json::from_value(a["lock_based_finality"]["block_committed_in_round_3"].clone())
                .unwrap();
        assert_eq!(locking.commit_round, Some(3));
        Ledger::replay(config.clone(), vec![locking.clone()]).unwrap();
        let mut wrong_round = locking;
        wrong_round.commit_round = Some(2);
        assert!(Ledger::replay(config, vec![wrong_round]).is_err());
    }

    #[test]
    fn the_companion_chain_replays_and_balances_add_up() {
        let chain = crate::companion_vectors::chain();
        let replayed = Ledger::replay(chain.config.clone(), chain.blocks.clone()).unwrap();
        assert_eq!(replayed.head_hash(), chain.ledger.head_hash());
        // The validator set changed at height 12, so the last block needed
        // three attestations and the first ten needed two.
        assert_eq!(chain.blocks[0].attestations.len(), 2);
        assert_eq!(chain.blocks.last().unwrap().attestations.len(), 3);
        let supply: u64 = replayed.state().credit_balances().map(|(_, b)| *b).sum();
        assert_eq!(supply, replayed.state().credits_issued());
        assert_eq!(supply, 525);
    }
}
