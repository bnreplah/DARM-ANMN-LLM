use super::*;
use crate::api::tests_support as t;
use crate::companion_vectors;
use axum::http::StatusCode;
use darm_core::NodeIdentity;
use serde_json::{Value, json};

fn device() -> NodeIdentity {
    NodeIdentity::from_secret_bytes(&[9; 32])
}

fn now() -> u64 {
    darm_core::time::now_ms()
}

fn enroll_body(who: &NodeIdentity, chain: &Hash32, created_ms: u64) -> Value {
    let node = who.node_id();
    let message = enroll_message(chain, &node, "android", "test phone", created_ms);
    json!({
        "node": node,
        "platform": "android",
        "label": "test phone",
        "created_ms": created_ms,
        "signature": who.sign(&message),
    })
}

struct Signed {
    headers: Vec<(&'static str, String)>,
    body: Vec<u8>,
}

fn sign_request(
    who: &NodeIdentity,
    chain: &Hash32,
    method: &str,
    target: &str,
    body: Vec<u8>,
    ts: u64,
    nonce: [u8; 16],
) -> Signed {
    let node = who.node_id();
    let message = request_message(chain, &node, method, target, ts, &nonce, &body);
    let nonce_hex: String = nonce.iter().map(|b| format!("{b:02x}")).collect();
    Signed {
        headers: vec![
            ("x-darm-node", node.to_hex()),
            ("x-darm-timestamp", ts.to_string()),
            ("x-darm-nonce", nonce_hex),
            ("x-darm-signature", who.sign(&message).to_tagged_string()),
        ],
        body,
    }
}

fn claim(chain: &darm_core::ledger::Ledger) -> WorkClaim {
    WorkClaim {
        kind: KIND_LEDGER_VERIFY.into(),
        from_height: 1,
        to_height: chain.height(),
        head: chain.head_hash(),
        verdict: Verdict::Ok,
        elapsed_ms: 50,
        completed_ms: now(),
    }
}

#[tokio::test]
async fn enrollment_is_proof_of_possession_bound_to_the_chain() {
    let dir = tempfile::tempdir().unwrap();
    let (app, state) = t::app(dir.path());
    let chain = state.archive.lock().unwrap().ledger.chain_id();
    let me = device();

    let (status, record) = t::send(
        &app,
        "POST",
        "/v1/devices",
        &[],
        Some(enroll_body(&me, &chain, now())),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{record}");
    assert_eq!(record["node"], json!(me.node_id()));

    // Idempotent for the identical claim; a different claim cannot rewrite it.
    let body = enroll_body(&me, &chain, record["created_ms"].as_u64().unwrap());
    let (status, _) = t::send(&app, "POST", "/v1/devices", &[], Some(body)).await;
    assert_eq!(status, StatusCode::OK);
    let (status, _) = t::send(
        &app,
        "POST",
        "/v1/devices",
        &[],
        Some(enroll_body(&me, &chain, now() + 1)),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT);

    // Someone else's key, signed by the wrong key, is refused.
    let other = NodeIdentity::from_secret_bytes(&[11; 32]);
    let mut forged = enroll_body(&other, &chain, now());
    forged["signature"] = enroll_body(&me, &chain, now())["signature"].clone();
    let (status, _) = t::send(&app, "POST", "/v1/devices", &[], Some(forged)).await;
    assert_eq!(status, StatusCode::FORBIDDEN);

    // A signature made for another chain does not enroll here.
    let foreign = Hash32::digest("chain", b"elsewhere");
    let (status, _) = t::send(
        &app,
        "POST",
        "/v1/devices",
        &[],
        Some(enroll_body(&other, &foreign, now())),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);

    // Stale or far-future enrollments are refused, as are unknown platforms.
    let (status, _) = t::send(
        &app,
        "POST",
        "/v1/devices",
        &[],
        Some(enroll_body(&other, &chain, now() - 10 * CLOCK_SKEW_MS)),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    let mut odd = enroll_body(&other, &chain, now());
    odd["platform"] = json!("toaster");
    let (status, _) = t::send(&app, "POST", "/v1/devices", &[], Some(odd)).await;
    assert_ne!(status, StatusCode::CREATED);

    let (status, public) = t::send(
        &app,
        "GET",
        &format!("/v1/devices/{}", me.node_id()),
        &[],
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(public["label"], "test phone");
    let (_, info) = t::send(&app, "GET", "/v1/info", &[], None).await;
    assert_eq!(info["devices"], 1);
}

#[tokio::test]
async fn an_enroll_token_gates_sign_ups_when_configured() {
    let dir = tempfile::tempdir().unwrap();
    let (_, mut state) = t::app(dir.path());
    state.enroll_token = Some(std::sync::Arc::from("enroll-token-0123456789abcdef012345"));
    let app = crate::api::router(state.clone());
    let chain = state.archive.lock().unwrap().ledger.chain_id();
    let body = enroll_body(&device(), &chain, now());
    let (status, _) = t::send(&app, "POST", "/v1/devices", &[], Some(body.clone())).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    let auth = [(
        "authorization",
        "Bearer enroll-token-0123456789abcdef012345".to_string(),
    )];
    let (status, _) = t::send(&app, "POST", "/v1/devices", &auth, Some(body)).await;
    assert_eq!(status, StatusCode::CREATED);
}

#[tokio::test]
async fn signed_requests_cannot_be_replayed_redirected_or_altered() {
    let dir = tempfile::tempdir().unwrap();
    let (app, state) = t::app(dir.path());
    let (chain, claim_now) = {
        let archive = state.archive.lock().unwrap();
        (archive.ledger.chain_id(), claim(&archive.ledger))
    };
    let me = device();
    t::send(
        &app,
        "POST",
        "/v1/devices",
        &[],
        Some(enroll_body(&me, &chain, now())),
    )
    .await;

    // A receipt for an empty chain: the archive has no block 1, so an `ok`
    // verdict is refused. Feed blocks first, in the next test; here use the
    // `invalid` verdict, which needs no head.
    let node = me.node_id();
    let target = format!("/v1/devices/{node}/receipts");
    let mut dispute = claim_now;
    dispute.verdict = Verdict::Invalid;
    dispute.to_height = 3;
    let message = work_message(&chain, &node, &dispute);
    let work = json!({"node": node, "kind": dispute.kind, "from_height": 1, "to_height": 3,
        "head": dispute.head, "verdict": "invalid", "elapsed_ms": 50,
        "completed_ms": dispute.completed_ms, "signature": me.sign(&message)});
    let body = serde_json::to_vec(&work).unwrap();

    let good = sign_request(&me, &chain, "POST", &target, body.clone(), now(), [1; 16]);
    let send = |s: &Signed| {
        let headers: Vec<(&str, String)> = s.headers.iter().map(|(k, v)| (*k, v.clone())).collect();
        let body: Value = serde_json::from_slice(&s.body).unwrap();
        let app = app.clone();
        let target = target.clone();
        async move { t::send(&app, "POST", &target, &headers, Some(body)).await }
    };
    let (status, saved) = send(&good).await;
    assert_eq!(status, StatusCode::CREATED, "{saved}");

    // Replaying the identical request is refused (nonce spent).
    let (status, _) = send(&good).await;
    assert_eq!(status, StatusCode::FORBIDDEN);

    // Stale timestamp.
    let stale = sign_request(
        &me,
        &chain,
        "POST",
        &target,
        body.clone(),
        now() - 10 * CLOCK_SKEW_MS,
        [2; 16],
    );
    assert_eq!(send(&stale).await.0, StatusCode::FORBIDDEN);

    // A signature for a different target does not authorise this one.
    let elsewhere = sign_request(
        &me,
        &chain,
        "POST",
        "/v1/devices/other/receipts",
        body.clone(),
        now(),
        [3; 16],
    );
    assert_eq!(send(&elsewhere).await.0, StatusCode::FORBIDDEN);

    // A different body under the same signature is refused.
    let mut tampered = sign_request(&me, &chain, "POST", &target, body.clone(), now(), [4; 16]);
    let mut altered: Value = serde_json::from_slice(&body).unwrap();
    altered["elapsed_ms"] = json!(1);
    tampered.body = serde_json::to_vec(&altered).unwrap();
    assert_eq!(send(&tampered).await.0, StatusCode::FORBIDDEN);

    // Another enrolled device cannot act for this one.
    let thief = NodeIdentity::from_secret_bytes(&[12; 32]);
    t::send(
        &app,
        "POST",
        "/v1/devices",
        &[],
        Some(enroll_body(&thief, &chain, now())),
    )
    .await;
    let stolen = sign_request(&thief, &chain, "POST", &target, body, now(), [5; 16]);
    assert_eq!(send(&stolen).await.0, StatusCode::FORBIDDEN);

    // A device that never enrolled is refused outright.
    let stranger = NodeIdentity::from_secret_bytes(&[13; 32]);
    let s_target = format!("/v1/devices/{}/receipts", stranger.node_id());
    let unknown = sign_request(
        &stranger,
        &chain,
        "POST",
        &s_target,
        b"{}".to_vec(),
        now(),
        [6; 16],
    );
    let headers: Vec<(&str, String)> = unknown
        .headers
        .iter()
        .map(|(k, v)| (*k, v.clone()))
        .collect();
    let (status, _) = t::send(&app, "POST", &s_target, &headers, Some(json!({}))).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn wallet_activity_and_receipts_follow_the_ledger() {
    let dir = tempfile::tempdir().unwrap();
    let chain = companion_vectors::chain();
    // The archive must run the same schedule and validator set as the chain.
    let (app, state) = t::app_for_chain(dir.path(), &chain.config);
    let (status, _) = t::send(
        &app,
        "POST",
        "/v1/ledger/blocks",
        &[("authorization", format!("Bearer {}", t::TOKEN))],
        Some(serde_json::to_value(&chain.blocks).unwrap()),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    let me = &chain.cast.device;
    let node = me.node_id();
    let (status, wallet) = t::send(
        &app,
        "GET",
        &format!("/v1/devices/{node}/wallet"),
        &[],
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    // 25 granted + 120 settled.
    assert_eq!(wallet["credits"]["balance"], 145);
    assert_eq!(wallet["credits"]["granted"], 25);
    assert_eq!(wallet["credits"]["settled_in"], 120);
    assert_eq!(wallet["credits"]["settled_out"], 0);
    assert_eq!(wallet["registered_on_ledger"], true);
    assert!(wallet["device"].is_null());

    // The coordinator paid out of what it was granted.
    let c = chain.cast.coordinator.node_id();
    let (_, coordinator) =
        t::send(&app, "GET", &format!("/v1/devices/{c}/wallet"), &[], None).await;
    assert_eq!(coordinator["credits"]["balance"], 300);
    assert_eq!(coordinator["credits"]["settled_out"], 200);

    // Activity is newest first and shows signed deltas.
    let (_, feed) = t::send(
        &app,
        "GET",
        &format!("/v1/devices/{node}/activity?limit=10"),
        &[],
        None,
    )
    .await;
    let items = feed["items"].as_array().unwrap();
    assert_eq!(items.len(), 3, "registration, grant, settlement: {feed}");
    assert_eq!(items[0]["kind"], "compute_settle");
    assert_eq!(items[0]["delta"], 120);
    assert_eq!(items[1]["kind"], "credit_grant");
    assert_eq!(items[2]["kind"], "node_registration");
    let sum: i64 = items.iter().map(|i| i["delta"].as_i64().unwrap()).sum();
    assert_eq!(sum, 145, "deltas add up to the balance");
    let (_, older) = t::send(
        &app,
        "GET",
        &format!("/v1/devices/{node}/activity?before=7"),
        &[],
        None,
    )
    .await;
    assert_eq!(older["items"].as_array().unwrap().len(), 2);

    // Enroll and submit a verification receipt for the real chain.
    let chain_id = state.archive.lock().unwrap().ledger.chain_id();
    let (status, _) = t::send(
        &app,
        "POST",
        "/v1/devices",
        &[],
        Some(enroll_body(me, &chain_id, now())),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    let target = format!("/v1/devices/{node}/receipts");
    let mut honest = claim(&state.archive.lock().unwrap().ledger);
    let receipt = |claim: &WorkClaim| {
        let message = work_message(&chain_id, &node, claim);
        serde_json::to_vec(
            &json!({"node": node, "kind": claim.kind, "from_height": claim.from_height,
            "to_height": claim.to_height, "head": claim.head, "verdict": claim.verdict,
            "elapsed_ms": claim.elapsed_ms, "completed_ms": claim.completed_ms,
            "signature": me.sign(&message)}),
        )
        .unwrap()
    };
    let submit = |body: Vec<u8>, nonce: u8| {
        let signed = sign_request(me, &chain_id, "POST", &target, body, now(), [nonce; 16]);
        let headers: Vec<(&str, String)> = signed
            .headers
            .iter()
            .map(|(k, v)| (*k, v.clone()))
            .collect();
        let value: Value = serde_json::from_slice(&signed.body).unwrap();
        let (app, target) = (app.clone(), target.clone());
        async move { t::send(&app, "POST", &target, &headers, Some(value)).await }
    };
    assert_eq!(submit(receipt(&honest), 1).await.0, StatusCode::CREATED);

    // A claimed head the archive does not have is refused, even if signed.
    honest.head = Hash32::digest("lie", b"head");
    assert_eq!(
        submit(receipt(&honest), 2).await.0,
        StatusCode::UNPROCESSABLE_ENTITY
    );
    // As is a range beyond the archive's chain.
    honest.head = chain.ledger.head_hash();
    honest.to_height += 5;
    assert_eq!(
        submit(receipt(&honest), 3).await.0,
        StatusCode::UNPROCESSABLE_ENTITY
    );

    let (_, list) = t::send(&app, "GET", &target, &[], None).await;
    assert_eq!(list["receipts"].as_array().unwrap().len(), 1);
    let (_, wallet) = t::send(
        &app,
        "GET",
        &format!("/v1/devices/{node}/wallet"),
        &[],
        None,
    )
    .await;
    assert_eq!(wallet["receipts"]["count"], 1);
}

#[test]
fn the_device_log_survives_restarts_and_rejects_tampering() {
    let dir = tempfile::tempdir().unwrap();
    let chain_id = Hash32::digest("chain", b"log-test");
    let me = device();
    {
        let mut devices = Devices::open(chain_id, dir.path()).unwrap();
        let node = me.node_id();
        let created = now();
        let message = enroll_message(&chain_id, &node, "ios", "kept", created);
        devices
            .enroll(
                node,
                "ios".into(),
                "kept".into(),
                created,
                me.sign(&message),
                now(),
            )
            .unwrap();
    }
    let reopened = Devices::open(chain_id, dir.path()).unwrap();
    assert_eq!(reopened.len(), 1);
    assert_eq!(reopened.get(&me.node_id()).unwrap().label, "kept");

    // Editing a record on disk breaks its signature, and startup refuses.
    let path = dir
        .path()
        .join(format!("devices-{}.jsonl", chain_id.short()));
    let text = std::fs::read_to_string(&path)
        .unwrap()
        .replace("kept", "evil");
    std::fs::write(&path, text).unwrap();
    assert!(Devices::open(chain_id, dir.path()).is_err());
}

#[test]
fn the_replay_cache_is_bounded_and_pruned() {
    let chain = Hash32::digest("chain", b"nonce-test");
    let me = device();
    let mut devices = Devices::new(chain);
    let node = me.node_id();
    let created = now();
    let sig = me.sign(&enroll_message(&chain, &node, "web", "", created));
    devices
        .enroll(node, "web".into(), String::new(), created, sig, created)
        .unwrap();
    let auth = |n: u64| {
        let mut nonce = [0u8; 16];
        nonce[..8].copy_from_slice(&n.to_be_bytes());
        let message = request_message(&chain, &node, "GET", "/x", created, &nonce, b"");
        RequestAuth {
            node,
            timestamp_ms: created,
            nonce,
            signature: me.sign(&message),
        }
    };
    devices
        .authenticate(&auth(1), "GET", "/x", b"", created)
        .unwrap();
    assert!(
        devices
            .authenticate(&auth(1), "GET", "/x", b"", created)
            .is_err()
    );
    // Expired nonces are forgotten once the cache is half full.
    for n in 2..=(MAX_NONCES as u64 / 2 + 1) {
        devices.nonces.insert(
            (node, {
                let mut x = [0u8; 16];
                x[..8].copy_from_slice(&n.to_be_bytes());
                x
            }),
            0,
        );
    }
    devices
        .authenticate(&auth(999_999), "GET", "/x", b"", created)
        .unwrap();
    assert!(devices.nonces.len() < 100, "expired entries were pruned");
}

#[tokio::test]
async fn companions_may_post_signed_requests_but_not_reach_token_gated_writes() {
    let dir = tempfile::tempdir().unwrap();
    let (_, state) = t::app(dir.path());
    let cors = crate::api::cors(&["https://me.github.io".to_string()]).unwrap();
    let app = crate::api::router_with_cors(state, cors);
    let preflight = |path: &'static str, method: &'static str, headers: &'static str| {
        let app = app.clone();
        async move {
            use tower::ServiceExt as _;
            let request = axum::http::Request::builder()
                .method("OPTIONS")
                .uri(path)
                .header("origin", "https://me.github.io")
                .header("access-control-request-method", method)
                .header("access-control-request-headers", headers)
                .body(axum::body::Body::empty())
                .unwrap();
            let response = app.oneshot(request).await.unwrap();
            let allow = |name: &str| {
                response
                    .headers()
                    .get(name)
                    .and_then(|v| v.to_str().ok())
                    .unwrap_or("")
                    .to_ascii_lowercase()
            };
            (
                allow("access-control-allow-methods"),
                allow("access-control-allow-headers"),
            )
        }
    };
    let (methods, headers) = preflight(
        "/v1/devices",
        "POST",
        "content-type,x-darm-node,x-darm-signature",
    )
    .await;
    assert!(methods.contains("post"), "{methods}");
    assert!(headers.contains("x-darm-signature"), "{headers}");
    // The ledger and memory writes keep the read-only policy: a page cannot
    // pass a preflight to POST there.
    let (methods, _) = preflight("/v1/ledger/blocks", "POST", "authorization").await;
    assert!(!methods.contains("post"), "{methods}");
    let (methods, _) = preflight("/v1/memories", "POST", "authorization").await;
    assert!(!methods.contains("post"), "{methods}");
}
