//! Test vectors for device companions (spec §11) and a multi-block chain that
//! exercises every rule a light verifier needs: two validators, a validator
//! set change, governance-approved grants and a compute settlement.
//!
//! The chain is built by the real [`Ledger`], so it is valid by construction;
//! the companion app (JavaScript) must verify it and derive the same
//! balances.

use crate::devices::wallet;
use crate::devices::{
    KIND_LEDGER_VERIFY, Verdict, WorkClaim, enroll_message, request_message, work_message,
};
use darm_core::ledger::{
    Block, CREDIT_GRANT_KIND, ChainConfig, EntryBody, Feature, Ledger, LedgerEntry, Member,
    Payment, SignedEntry, ValidatorSet, credit_grant_payload, validator_set_payload,
};
use darm_core::{Hash32, NodeId, NodeIdentity};
use serde_json::{Value, json};
use std::collections::BTreeMap;

const START_MS: u64 = 1_700_000_000_000;

struct Builder {
    ledger: Ledger,
    identities: BTreeMap<NodeId, NodeIdentity>,
    clock: u64,
    nonce: u64,
}

impl Builder {
    fn new(config: ChainConfig, seeds: &[u8]) -> Self {
        Self {
            ledger: Ledger::new(config),
            identities: seeds.iter().map(|s| (id(*s).node_id(), id(*s))).collect(),
            clock: START_MS,
            nonce: 1_000,
        }
    }

    /// Submit the entries, then have the designated proposer propose and
    /// every validator in force attest until the block is final.
    fn block(&mut self, entries: Vec<(&NodeIdentity, EntryBody)>) -> Vec<Hash32> {
        let chain = self.ledger.chain_id();
        let mut ids = Vec::new();
        for (author, body) in entries {
            self.nonce += 1;
            let entry = LedgerEntry {
                chain,
                author: author.node_id(),
                timestamp_ms: self.clock,
                nonce: self.nonce,
                body,
            };
            let signed = SignedEntry {
                signature: author.sign(&entry.canonical()),
                entry,
            };
            ids.push(signed.id());
            self.ledger
                .submit(signed, self.clock)
                .expect("entry admitted");
        }
        let proposer = self.ledger.proposer_for_next(0);
        let proposer = &self.identities[&proposer];
        let block = self
            .ledger
            .propose(proposer, 0, self.clock)
            .expect("designated proposer")
            .expect("block has entries");
        let voters: Vec<NodeId> = self
            .ledger
            .validators()
            .members()
            .map(|(n, _)| *n)
            .collect();
        for voter in voters {
            let (_, finalized) = self
                .ledger
                .attest(&self.identities[&voter], &block.hash())
                .expect("attestation");
            if finalized.is_some() {
                break;
            }
        }
        self.clock += 1_000;
        ids
    }
}

fn id(seed: u8) -> NodeIdentity {
    NodeIdentity::from_secret_bytes(&[seed; 32])
}

fn registration(who: &NodeIdentity, role: &str) -> EntryBody {
    EntryBody::NodeRegistration {
        role: role.into(),
        genome: Hash32::digest("vector-genome", who.node_id().as_bytes()),
        generation: 0,
        parent: None,
    }
}

/// The chain, its blocks and what a verifier must derive from them.
pub struct Chain {
    pub config: ChainConfig,
    pub blocks: Vec<Block>,
    pub cast: Cast,
    pub ledger: Ledger,
}

pub struct Cast {
    pub v1: NodeIdentity,
    pub v2: NodeIdentity,
    pub v3: NodeIdentity,
    pub coordinator: NodeIdentity,
    pub device: NodeIdentity,
    pub worker: NodeIdentity,
}

pub fn chain() -> Chain {
    let cast = Cast {
        v1: id(1),
        v2: id(2),
        v3: id(4),
        coordinator: id(3),
        device: id(9),
        worker: id(10),
    };
    let config = ChainConfig {
        name: "darm-companion-vectors".into(),
        validators: ValidatorSet::equal([cast.v1.node_id(), cast.v2.node_id()])
            .expect("validator set"),
        activations: [(Feature::Credits, 0), (Feature::Governance, 0)].into(),
    };
    let mut b = Builder::new(config.clone(), &[1, 2, 4, 3, 9, 10]);
    let (c, d, w) = (&cast.coordinator, &cast.device, &cast.worker);
    let deadline = START_MS + 10_000_000;
    let memory = Hash32::digest("vector-memory", b"1");

    // h1: nodes announce themselves.
    b.block(vec![
        (c, registration(c, "coordinator")),
        (d, registration(d, "phone")),
        (w, registration(w, "worker")),
    ]);

    // h2: a memory commit and the first grant proposal.
    let grant_c = b.block(vec![
        (
            &cast.v1,
            EntryBody::MemoryCommit {
                memory,
                importance_milli: 700,
            },
        ),
        (
            &cast.v1,
            EntryBody::Proposal {
                kind: CREDIT_GRANT_KIND.into(),
                payload: credit_grant_payload(&c.node_id(), 500),
                deadline_ms: deadline,
            },
        ),
    ]);
    let grant_c_proposal = grant_c[1];
    // h3: both validators vote.
    let vote = |proposal| EntryBody::Vote {
        proposal,
        support: true,
    };
    b.block(vec![
        (&cast.v1, vote(grant_c_proposal)),
        (&cast.v2, vote(grant_c_proposal)),
    ]);
    // h4: grant to the coordinator; proposal for the device.
    let h4 = b.block(vec![
        (
            &cast.v1,
            EntryBody::CreditGrant {
                proposal: grant_c_proposal,
                to: c.node_id(),
                amount: 500,
            },
        ),
        (
            &cast.v1,
            EntryBody::Proposal {
                kind: CREDIT_GRANT_KIND.into(),
                payload: credit_grant_payload(&d.node_id(), 25),
                deadline_ms: deadline,
            },
        ),
    ]);
    let grant_d_proposal = h4[1];
    // h5: votes. h6: grant to the device and the compute commit.
    b.block(vec![
        (&cast.v1, vote(grant_d_proposal)),
        (&cast.v2, vote(grant_d_proposal)),
    ]);
    let job = Hash32::digest("vector-job", b"companion");
    let receipts_root = Hash32::digest("vector-receipts", b"companion");
    b.block(vec![
        (
            &cast.v1,
            EntryBody::CreditGrant {
                proposal: grant_d_proposal,
                to: d.node_id(),
                amount: 25,
            },
        ),
        (
            c,
            EntryBody::ComputeCommit {
                job,
                result_root: Hash32::digest("vector-result", b"companion"),
                receipts_root,
                shards: 4,
                workers: 2,
            },
        ),
    ]);
    // h7: settlement, a reputation report and a revoked memory.
    b.block(vec![
        (
            c,
            EntryBody::ComputeSettle {
                job,
                receipts_root,
                payments: vec![
                    Payment {
                        worker: d.node_id(),
                        amount: 120,
                    },
                    Payment {
                        worker: w.node_id(),
                        amount: 80,
                    },
                ],
            },
        ),
        (
            &cast.v1,
            EntryBody::ReputationReport {
                subject: d.node_id(),
                delta_milli: 50,
                evidence: Hash32::digest("vector-evidence", b"1"),
            },
        ),
        (&cast.v1, EntryBody::MemoryRevoke { memory }),
    ]);
    // h8-h10: governance adds a third validator from height 12.
    let mut members = vec![
        Member {
            node: cast.v1.node_id(),
            weight: 1,
        },
        Member {
            node: cast.v2.node_id(),
            weight: 1,
        },
        Member {
            node: cast.v3.node_id(),
            weight: 1,
        },
    ];
    members.sort_by_key(|m| m.node);
    let effective = 12;
    let h8 = b.block(vec![(
        &cast.v1,
        EntryBody::Proposal {
            kind: "validator-set".into(),
            payload: validator_set_payload(&members, effective),
            deadline_ms: deadline,
        },
    )]);
    b.block(vec![(&cast.v1, vote(h8[0])), (&cast.v2, vote(h8[0]))]);
    b.block(vec![(
        &cast.v1,
        EntryBody::ValidatorSetChange {
            proposal: h8[0],
            members,
            effective_height: effective,
        },
    )]);
    // h11 is still under the old set; h12 needs all three validators.
    for n in 0..2u8 {
        b.block(vec![(
            &cast.v1,
            EntryBody::MemoryCommit {
                memory: Hash32::digest("vector-memory", &[2 + n]),
                importance_milli: 100,
            },
        )]);
    }
    let blocks = b.ledger.blocks().to_vec();
    Chain {
        config,
        blocks,
        cast,
        ledger: b.ledger,
    }
}

fn balances(chain: &Chain) -> Value {
    let cast = &chain.cast;
    let mut out = serde_json::Map::new();
    for (name, who) in [
        ("coordinator", &cast.coordinator),
        ("device", &cast.device),
        ("worker", &cast.worker),
        ("v1", &cast.v1),
    ] {
        let node = who.node_id();
        let t = wallet::totals(&chain.ledger, &node);
        out.insert(
            name.into(),
            json!({
                "node": node,
                "balance": chain.ledger.state().credit_balance(&node),
                "granted": t.granted,
                "settled_in": t.settled_in,
                "settled_out": t.settled_out,
                "entries_authored": t.entries_authored,
            }),
        );
    }
    Value::Object(out)
}

pub fn generate() -> Value {
    let chain = chain();
    let device = id(9);
    let chain_id = chain.ledger.chain_id();
    let node = device.node_id();
    let hex = |bytes: &[u8]| bytes.iter().map(|b| format!("{b:02x}")).collect::<String>();

    let enroll = enroll_message(&chain_id, &node, "ios", "ben-phone", START_MS);
    let nonce = [0xab_u8; 16];
    let body = br#"{"hello":"device"}"#;
    let target = "/v1/devices/receipts?x=1";
    let request = request_message(&chain_id, &node, "POST", target, START_MS + 5, &nonce, body);
    let claim = WorkClaim {
        kind: KIND_LEDGER_VERIFY.into(),
        from_height: 1,
        to_height: chain.ledger.height(),
        head: chain.ledger.head_hash(),
        verdict: Verdict::Ok,
        elapsed_ms: 1234,
        completed_ms: START_MS + 9,
    };
    let work = work_message(&chain_id, &node, &claim);
    let members: Vec<Value> = chain
        .config
        .validators
        .members()
        .map(|(n, w)| json!({"node": n, "weight": w}))
        .collect();

    json!({
        "note": "Chain built by the real ledger: two validators, a validator-set change effective at height 12, grants, a settlement.",
        "device_seed": "09 repeated 32 times",
        "chain": {
            "name": chain.config.name,
            "chain_id": chain_id,
            "genesis_validators": members,
            "activations": {"credits": 0, "governance": 0},
            "height": chain.ledger.height(),
            "head": chain.ledger.head_hash(),
            "blocks": chain.blocks,
        },
        "expected": balances(&chain),
        "device": {
            "node": node,
            "enroll": {
                "platform": "ios",
                "label": "ben-phone",
                "created_ms": START_MS,
                "message": hex(&enroll),
                "signature": device.sign(&enroll),
            },
            "request": {
                "method": "POST",
                "target": target,
                "timestamp_ms": START_MS + 5,
                "nonce": hex(&nonce),
                "body": String::from_utf8_lossy(body),
                "body_hash": crate::devices::body_hash(body),
                "message": hex(&request),
                "signature": device.sign(&request),
            },
            "work": {
                "claim": claim,
                "message": hex(&work),
                "signature": device.sign(&work),
            },
        },
    })
}
