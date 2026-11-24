use super::*;
use crate::archive::{Archive, ArchiveConfig};
use axum::body::Body;
use axum::http::Request;
use darm_core::NodeIdentity;
use darm_core::ledger::{EntryBody, Ledger, SignedEntry};
use darm_memory::{MemoryKind, MemoryRecord, Scope};
use http_body_util::BodyExt;
use tower::ServiceExt;

const TOKEN: &str = "0123456789abcdef0123456789abcdef";

fn validator() -> NodeIdentity {
    NodeIdentity::from_secret_bytes(&[5; 32])
}

fn app(dir: &std::path::Path) -> (Router, AppState) {
    app_with(dir, Default::default())
}

fn app_with(
    dir: &std::path::Path,
    activations: std::collections::BTreeMap<darm_core::ledger::Feature, u64>,
) -> (Router, AppState) {
    let archive = Archive::open(ArchiveConfig {
        data_dir: Some(dir.to_path_buf()),
        chain_name: "archive-test".into(),
        validators: vec![validator().node_id()],
        activations,
    })
    .unwrap();
    let devices = crate::devices::Devices::open(archive.ledger.chain_id(), dir).unwrap();
    let state = AppState {
        archive: Arc::new(Mutex::new(archive)),
        write_token: Some(Arc::from(TOKEN)),
        read_token: None,
        devices: Arc::new(Mutex::new(devices)),
        enroll_token: None,
    };
    (router(state.clone()), state)
}

async fn call(
    app: &Router,
    method: &str,
    uri: &str,
    token: Option<&str>,
    body: Option<Value>,
) -> (StatusCode, Value) {
    let mut request = Request::builder().method(method).uri(uri);
    if let Some(token) = token {
        request = request.header("authorization", format!("Bearer {token}"));
    }
    let request = match body {
        Some(body) => request
            .header("content-type", "application/json")
            .body(Body::from(serde_json::to_vec(&body).unwrap()))
            .unwrap(),
        None => request.body(Body::empty()).unwrap(),
    };
    let response = app.clone().oneshot(request).await.unwrap();
    let status = response.status();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    let value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    (status, value)
}

/// Produce finalized blocks with a real validator, as a mesh node would.
fn finalized_blocks(memory: &SignedMemory) -> Vec<Block> {
    let v = validator();
    let mut ledger = Ledger::new(darm_core::ledger::ChainConfig::new(
        "archive-test",
        darm_core::ledger::ValidatorSet::equal([v.node_id()]).unwrap(),
    ));
    let entry = SignedEntry::sign(
        &v,
        ledger.chain_id(),
        EntryBody::MemoryCommit {
            memory: memory.id(),
            importance_milli: memory.record.importance_milli,
        },
        now_ms(),
    )
    .unwrap();
    ledger.submit(entry, now_ms()).unwrap();
    let block = ledger.propose(&v, 0, now_ms()).unwrap().unwrap();
    ledger.attest(&v, &block.hash()).unwrap();
    ledger.blocks().to_vec()
}

fn memory(scope: Scope) -> SignedMemory {
    SignedMemory::sign(
        &validator(),
        MemoryRecord {
            author: validator().node_id(),
            created_ms: now_ms(),
            kind: MemoryKind::Procedural,
            scope,
            importance_milli: 600,
            valence_milli: 0,
            tags: vec!["ns:default".into()],
            content: "Rotate validator keys with darm-node keygen and a validators proposal".into(),
            parent: None,
        },
    )
    .unwrap()
}

#[tokio::test]
async fn end_to_end_archive_flow() {
    let dir = tempfile::tempdir().unwrap();
    let (app, _) = app(dir.path());
    let mem = memory(Scope::Mesh);
    let blocks = finalized_blocks(&mem);

    let (status, _) = call(&app, "POST", "/v1/ledger/blocks", None, Some(json!(blocks))).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    let (status, _) = call(
        &app,
        "POST",
        "/v1/ledger/blocks",
        Some("wrong-token-wrong-token-wrong-tok"),
        Some(json!(blocks)),
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    let (status, body) = call(
        &app,
        "POST",
        "/v1/ledger/blocks",
        Some(TOKEN),
        Some(json!(blocks)),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["height"], 1);

    let (status, body) = call(
        &app,
        "POST",
        "/v1/memories",
        Some(TOKEN),
        Some(json!([mem])),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body[0]["stored"], true);
    assert_eq!(body[0]["finalized"], true);

    let (status, body) = call(
        &app,
        "POST",
        "/v1/memories/search",
        None,
        Some(json!({"query": "rotate validator keys"})),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["hits"].as_array().unwrap().len(), 1);

    let (_, body) = call(
        &app,
        "GET",
        &format!("/v1/memories/{}", mem.id()),
        None,
        None,
    )
    .await;
    assert_eq!(body["record"]["content"], mem.record.content);

    let entry_id = blocks[0].entries[0].id();
    let (status, proof) = call(
        &app,
        "GET",
        &format!("/v1/ledger/entries/{entry_id}/proof"),
        None,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let parsed: darm_core::merkle::Proof = serde_json::from_value(proof["proof"].clone()).unwrap();
    let root = Hash32::from_hex(proof["entries_root"].as_str().unwrap()).unwrap();
    assert!(darm_core::merkle::verify(&root, &entry_id, &parsed));

    let (_, checkpoint) = call(&app, "GET", "/v1/ledger/checkpoint", None, None).await;
    assert_eq!(checkpoint["height"], 1);
    assert!(checkpoint["block_hash"].as_str().unwrap().starts_with("0x"));

    // State survives a restart and is re-verified.
    drop(app);
    let (app, _) = self::app(dir.path());
    let (_, info) = call(&app, "GET", "/v1/info", None, None).await;
    assert_eq!(info["height"], 1);
    assert_eq!(info["memory"]["finalized"], 1);
}

#[tokio::test]
async fn rejects_forgeries_and_private_memories() {
    let dir = tempfile::tempdir().unwrap();
    let (app, _) = app(dir.path());
    let mut blocks = finalized_blocks(&memory(Scope::Mesh));
    blocks[0].attestations.clear();
    let (status, _) = call(
        &app,
        "POST",
        "/v1/ledger/blocks",
        Some(TOKEN),
        Some(json!(blocks)),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::UNPROCESSABLE_ENTITY,
        "quorum is required even with the token"
    );

    let mut forged = memory(Scope::Mesh);
    forged.record.content.push('!');
    let (_, body) = call(
        &app,
        "POST",
        "/v1/memories",
        Some(TOKEN),
        Some(json!([forged])),
    )
    .await;
    assert!(body[0]["error"].as_str().unwrap().contains("signature"));

    let (_, body) = call(
        &app,
        "POST",
        "/v1/memories",
        Some(TOKEN),
        Some(json!([memory(Scope::Local)])),
    )
    .await;
    assert!(
        body[0]["error"].is_string(),
        "local-scope memories are never archived"
    );
}

#[tokio::test]
async fn read_only_without_write_token_and_optional_read_token() {
    let dir = tempfile::tempdir().unwrap();
    let (_, mut state) = app(dir.path());
    state.write_token = None;
    state.read_token = Some(Arc::from(TOKEN));
    let app = router(state);
    let (status, _) = call(&app, "POST", "/v1/memories", Some(TOKEN), Some(json!([]))).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    let (status, _) = call(&app, "GET", "/v1/info", None, None).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    let (status, _) = call(&app, "GET", "/v1/info", Some(TOKEN), None).await;
    assert_eq!(status, StatusCode::OK);
    let (status, _) = call(&app, "GET", "/healthz", None, None).await;
    assert_eq!(status, StatusCode::OK);
}

#[tokio::test]
async fn oversized_bodies_are_refused() {
    let dir = tempfile::tempdir().unwrap();
    let (app, _) = app(dir.path());
    let huge = "x".repeat(MAX_BODY_BYTES + 1);
    let (status, _) = call(
        &app,
        "POST",
        "/v1/memories/search",
        None,
        Some(json!({"query": huge})),
    )
    .await;
    assert_eq!(status, StatusCode::PAYLOAD_TOO_LARGE);
}

#[test]
fn constant_time_compare() {
    assert!(constant_time_eq(b"abc", b"abc"));
    assert!(!constant_time_eq(b"abc", b"abd"));
    assert!(!constant_time_eq(b"abc", b"abcd"));
}

async fn cors_headers(
    app: &Router,
    method: &str,
    origin: &str,
    preflight_for: Option<&str>,
) -> HeaderMap {
    let mut request = Request::builder()
        .method(method)
        .uri("/v1/info")
        .header("origin", origin);
    if let Some(wanted) = preflight_for {
        request = request.header("access-control-request-method", wanted);
    }
    let response = app
        .clone()
        .oneshot(request.body(Body::empty()).unwrap())
        .await
        .unwrap();
    response.headers().clone()
}

#[tokio::test]
async fn cors_lets_listed_origins_read_and_nobody_write() {
    const MONITOR: &str = "https://example.github.io";
    let dir = tempfile::tempdir().unwrap();
    let (plain, _) = app(dir.path());
    // Off by default: no CORS headers at all.
    let headers = cors_headers(&plain, "GET", MONITOR, None).await;
    assert!(headers.get("access-control-allow-origin").is_none());

    let cors = cors_layer(&[MONITOR.to_string()]).unwrap().unwrap();
    let app = plain.layer(cors);
    let headers = cors_headers(&app, "GET", MONITOR, None).await;
    assert_eq!(headers["access-control-allow-origin"], MONITOR);
    assert!(headers.get("access-control-allow-credentials").is_none());
    // Another site gets nothing a browser would accept.
    let headers = cors_headers(&app, "GET", "https://evil.example", None).await;
    assert!(headers.get("access-control-allow-origin").is_none());
    // Preflight never offers a writing method, even to the listed origin.
    let headers = cors_headers(&app, "OPTIONS", MONITOR, Some("POST")).await;
    let methods = headers["access-control-allow-methods"].to_str().unwrap();
    assert_eq!(methods, "GET");
}

#[test]
fn cors_origins_must_be_exact() {
    assert!(cors_layer(&[]).unwrap().is_none());
    for bad in [
        "*",
        "https://*",
        "https://a.example/path",
        "a.example",
        "https://",
        "ftp://a.example",
    ] {
        assert!(
            cors_layer(&[bad.to_string()]).is_err(),
            "{bad} should be refused"
        );
    }
    assert!(
        cors_layer(&["http://127.0.0.1:8080".into(), " https://a.example ".into()])
            .unwrap()
            .is_some()
    );
}

/// Like `finalized_blocks`, but finalized by lock-based BFT rounds: the
/// block carries `commit_round` and that round's precommits.
fn locking_blocks(memory: &SignedMemory) -> Vec<Block> {
    let v = validator();
    let mut ledger = Ledger::new(darm_core::ledger::ChainConfig::new(
        "archive-test",
        darm_core::ledger::ValidatorSet::equal([v.node_id()]).unwrap(),
    ));
    ledger.enable_locking();
    let entry = SignedEntry::sign(
        &v,
        ledger.chain_id(),
        EntryBody::MemoryCommit {
            memory: memory.id(),
            importance_milli: memory.record.importance_milli,
        },
        now_ms(),
    )
    .unwrap();
    ledger.submit(entry, now_ms()).unwrap();
    ledger.propose_round(&v, now_ms()).unwrap().unwrap();
    assert_eq!(ledger.height(), 1, "a sole validator decides in round 0");
    ledger.blocks().to_vec()
}

#[tokio::test]
async fn archive_verifies_blocks_finalized_by_lock_based_bft() {
    let dir = tempfile::tempdir().unwrap();
    let (app, _) = app(dir.path());
    let blocks = locking_blocks(&memory(Scope::Mesh));
    assert_eq!(blocks[0].commit_round, Some(0));

    // The certificate is precommits for this round. Presenting it as
    // one-vote attestations, or for another round, must not verify.
    let mut as_one_vote = blocks.clone();
    as_one_vote[0].commit_round = None;
    let mut other_round = blocks.clone();
    other_round[0].commit_round = Some(1);
    for forged in [as_one_vote, other_round] {
        let (status, _) = call(
            &app,
            "POST",
            "/v1/ledger/blocks",
            Some(TOKEN),
            Some(json!(forged)),
        )
        .await;
        assert_ne!(status, StatusCode::OK);
    }

    let (status, _) = call(
        &app,
        "POST",
        "/v1/ledger/blocks",
        Some(TOKEN),
        Some(json!(blocks)),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let (_, info) = call(&app, "GET", "/v1/info", None, None).await;
    assert_eq!(info["height"], 1);
    // The certificate round survives storage and is served back.
    let (_, served) = call(&app, "GET", "/v1/ledger/blocks?after=0", None, None).await;
    assert_eq!(served[0]["commit_round"], 0);
}

#[tokio::test]
async fn archive_enforces_the_activation_schedule() {
    use darm_core::ledger::Feature;
    use darm_core::ledger::locking::{Vote, VoteKind, vote_message};

    let schedule = crate::archive::parse_activations(&["evidence@0".into()]).unwrap();
    assert!(crate::archive::parse_activations(&["evidence".into()]).is_err());
    assert!(crate::archive::parse_activations(&["warp@1".into()]).is_err());

    // A chain with evidence active finalizes an Equivocation entry.
    let v = validator();
    let mut config = darm_core::ledger::ChainConfig::new(
        "archive-test",
        darm_core::ledger::ValidatorSet::equal([v.node_id()]).unwrap(),
    );
    config.activations = schedule.clone();
    let mut ledger = Ledger::new(config);
    let chain = ledger.chain_id();
    let vote = |block: darm_core::Hash32| Vote {
        kind: VoteKind::Prevote,
        height: 9,
        round: 0,
        block: Some(block),
        validator: v.node_id(),
        signature: v.sign(&vote_message(&chain, VoteKind::Prevote, 9, 0, Some(&block))),
    };
    let (mut first, mut second) = (
        vote(darm_core::Hash32::digest("b", b"1")),
        vote(darm_core::Hash32::digest("b", b"2")),
    );
    if second.block < first.block {
        std::mem::swap(&mut first, &mut second);
    }
    let entry = SignedEntry::sign(
        &v,
        chain,
        EntryBody::Equivocation { first, second },
        now_ms(),
    )
    .unwrap();
    ledger.submit(entry, now_ms()).unwrap();
    let block = ledger.propose(&v, 0, now_ms()).unwrap().unwrap();
    ledger.attest(&v, &block.hash()).unwrap();
    let blocks = ledger.blocks().to_vec();

    // An archive without the schedule rejects the block...
    let off = tempfile::tempdir().unwrap();
    let (app_off, _) = app(off.path());
    let (status, _) = call(
        &app_off,
        "POST",
        "/v1/ledger/blocks",
        Some(TOKEN),
        Some(json!(blocks)),
    )
    .await;
    assert_ne!(status, StatusCode::OK);

    // ...one with the same schedule accepts it and reports the schedule.
    let on = tempfile::tempdir().unwrap();
    let (app_on, _) = app_with(on.path(), schedule);
    let (status, _) = call(
        &app_on,
        "POST",
        "/v1/ledger/blocks",
        Some(TOKEN),
        Some(json!(blocks)),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let (_, info) = call(&app_on, "GET", "/v1/info", None, None).await;
    assert_eq!(info["height"], 1);
    assert_eq!(info["activations"]["evidence"], 0);
    let _ = Feature::Evidence;
}

#[tokio::test]
async fn info_reports_the_validator_set_in_force() {
    use darm_core::ledger::{Feature, Member, VALIDATOR_SET_KIND, validator_set_payload};

    let v = validator();
    let schedule: std::collections::BTreeMap<_, _> = [(Feature::Governance, 0)].into();
    let mut config = darm_core::ledger::ChainConfig::new(
        "archive-test",
        darm_core::ledger::ValidatorSet::equal([v.node_id()]).unwrap(),
    );
    config.activations = schedule.clone();
    let mut ledger = Ledger::new(config);
    let chain = ledger.chain_id();
    let start = now_ms();
    let mut clock = start;
    let mut block = |ledger: &mut Ledger, body: EntryBody| {
        clock += 1;
        let entry = SignedEntry::sign(&v, chain, body, clock).unwrap();
        let id = entry.id();
        ledger.submit(entry, clock).unwrap();
        let block = ledger.propose(&v, 0, clock).unwrap().unwrap();
        ledger.attest(&v, &block.hash()).unwrap();
        id
    };
    // Governance re-weights the single validator from height 5 on.
    let members = vec![Member {
        node: v.node_id(),
        weight: 5,
    }];
    let proposal = block(
        &mut ledger,
        EntryBody::Proposal {
            kind: VALIDATOR_SET_KIND.into(),
            payload: validator_set_payload(&members, 5),
            deadline_ms: start + 3_600_000,
        },
    );
    block(
        &mut ledger,
        EntryBody::Vote {
            proposal,
            support: true,
        },
    );
    block(
        &mut ledger,
        EntryBody::ValidatorSetChange {
            proposal,
            members,
            effective_height: 5,
        },
    );
    let dir = tempfile::tempdir().unwrap();
    let (app, _) = app_with(dir.path(), schedule);
    let post = |blocks: Vec<darm_core::ledger::Block>| {
        let app = app.clone();
        async move {
            call(
                &app,
                "POST",
                "/v1/ledger/blocks",
                Some(TOKEN),
                Some(json!(blocks)),
            )
            .await
            .0
        }
    };
    assert_eq!(post(ledger.blocks().to_vec()).await, StatusCode::OK);
    let (_, info) = call(&app, "GET", "/v1/info", None, None).await;
    assert_eq!(info["validators"][0]["weight"], 1, "not yet in force");

    block(
        &mut ledger,
        EntryBody::Proposal {
            kind: "note".into(),
            payload: darm_core::Hash32::digest("n", b"1"),
            deadline_ms: start + 3_600_000,
        },
    );
    assert_eq!(post(ledger.blocks()[3..].to_vec()).await, StatusCode::OK);
    let (_, info) = call(&app, "GET", "/v1/info", None, None).await;
    assert_eq!(info["height"], 4);
    assert_eq!(
        info["validators"][0]["weight"], 5,
        "the set for the next block"
    );
}
