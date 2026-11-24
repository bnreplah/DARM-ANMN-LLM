//! HTTP routes for device companions (spec §11).
//!
//! Reads are public like the rest of the archive (the ledger they report on
//! is public) and honour the read token when one is set. The two writes need
//! no bearer secret of the device's: enrollment is self-signed and receipt
//! submission is a signed request. Enrollment can additionally be gated by an
//! operator-issued token so a public archive is not open to bulk sign-ups.

use super::wallet;
use super::{RequestAuth, SignedWork, wallet::Totals};
use crate::api::{ApiError, ApiResult, AppState, check_token, read_auth};
use axum::body::Bytes;
use axum::extract::{DefaultBodyLimit, Path, Query, State};
use axum::http::{HeaderMap, Method, StatusCode, Uri};
use axum::routing::{get, post};
use axum::{Json, Router};
use darm_core::time::now_ms;
use darm_core::{DarmError, NodeId, Signature};
use serde::Deserialize;
use serde_json::{Value, json};

const MAX_BODY_BYTES: usize = 16 * 1024;
const DEFAULT_ACTIVITY: usize = 50;
const MAX_ACTIVITY: usize = 200;

pub fn routes(state: AppState) -> Router {
    Router::new()
        .route("/v1/devices", post(enroll))
        .route("/v1/devices/{node}", get(get_device))
        .route("/v1/devices/{node}/wallet", get(wallet_view))
        .route("/v1/devices/{node}/activity", get(activity_view))
        .route(
            "/v1/devices/{node}/receipts",
            get(list_receipts).post(post_receipt),
        )
        .layer(DefaultBodyLimit::max(MAX_BODY_BYTES))
        .with_state(state)
}

fn parse_node(value: &str) -> ApiResult<NodeId> {
    NodeId::from_hex(value).map_err(|e| ApiError(StatusCode::BAD_REQUEST, e.to_string()))
}

fn bad_json(error: &serde_json::Error) -> ApiError {
    ApiError(
        StatusCode::BAD_REQUEST,
        format!("invalid JSON body: {error}"),
    )
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct EnrollBody {
    node: NodeId,
    platform: String,
    #[serde(default)]
    label: String,
    created_ms: u64,
    signature: Signature,
}

async fn enroll(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Bytes,
) -> ApiResult<(StatusCode, Json<Value>)> {
    check_token(&headers, state.enroll_token.as_deref(), false)?;
    let request: EnrollBody = serde_json::from_slice(&body).map_err(|e| bad_json(&e))?;
    let mut devices = state.devices.lock().expect("devices lock");
    match devices.enroll(
        request.node,
        request.platform,
        request.label,
        request.created_ms,
        request.signature,
        now_ms(),
    ) {
        Ok((record, created)) => {
            let status = if created {
                StatusCode::CREATED
            } else {
                StatusCode::OK
            };
            Ok((status, Json(json!(record))))
        }
        Err(DarmError::Duplicate) => Err(ApiError(
            StatusCode::CONFLICT,
            "this key is already enrolled with a different claim".into(),
        )),
        Err(error) => Err(error.into()),
    }
}

async fn get_device(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(node): Path<String>,
) -> ApiResult<Json<Value>> {
    read_auth(&state, &headers)?;
    let node = parse_node(&node)?;
    let devices = state.devices.lock().expect("devices lock");
    match devices.get(&node) {
        Some(record) => Ok(Json(json!(record))),
        None => Err(ApiError(
            StatusCode::NOT_FOUND,
            "device not enrolled".into(),
        )),
    }
}

async fn wallet_view(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(node): Path<String>,
) -> ApiResult<Json<Value>> {
    read_auth(&state, &headers)?;
    let node = parse_node(&node)?;
    let archive = state.archive.lock().expect("archive lock");
    let devices = state.devices.lock().expect("devices lock");
    let ledger = &archive.ledger;
    let Totals {
        granted,
        settled_in,
        settled_out,
        entries_authored,
        settlements_received,
    } = wallet::totals(ledger, &node);
    let work = devices.work(&node);
    Ok(Json(json!({
        "node": node,
        "device": devices.get(&node),
        "registered_on_ledger": ledger.state().registration(&node).is_some(),
        "reputation": ledger.state().reputation(&node),
        "ledger": {"height": ledger.height(), "head": ledger.head_hash()},
        "credits": {
            "balance": ledger.state().credit_balance(&node),
            "granted": granted,
            "settled_in": settled_in,
            "settled_out": settled_out,
            "network_issued": ledger.state().credits_issued(),
            "closed_loop": true,
        },
        "entries_authored": entries_authored,
        "settlements_received": settlements_received,
        "receipts": {
            "count": work.len(),
            "last_completed_ms": work.iter().map(|w| w.claim.completed_ms).max(),
        },
    })))
}

#[derive(Deserialize)]
struct ActivityQuery {
    /// Only entries in blocks below this height (for paging back).
    before: Option<u64>,
    limit: Option<usize>,
}

async fn activity_view(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(node): Path<String>,
    Query(query): Query<ActivityQuery>,
) -> ApiResult<Json<Value>> {
    read_auth(&state, &headers)?;
    let node = parse_node(&node)?;
    let limit = query
        .limit
        .unwrap_or(DEFAULT_ACTIVITY)
        .clamp(1, MAX_ACTIVITY);
    let archive = state.archive.lock().expect("archive lock");
    let items = wallet::activity(&archive.ledger, &node, query.before, limit);
    Ok(Json(json!({
        "node": node,
        "height": archive.ledger.height(),
        "items": items,
    })))
}

async fn list_receipts(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(node): Path<String>,
) -> ApiResult<Json<Value>> {
    read_auth(&state, &headers)?;
    let node = parse_node(&node)?;
    let devices = state.devices.lock().expect("devices lock");
    Ok(Json(json!({"node": node, "receipts": devices.work(&node)})))
}

async fn post_receipt(
    State(state): State<AppState>,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
    Path(node): Path<String>,
    body: Bytes,
) -> ApiResult<(StatusCode, Json<Value>)> {
    let node = parse_node(&node)?;
    let auth = RequestAuth::parse(&headers)?;
    if auth.node != node {
        return Err(ApiError(
            StatusCode::FORBIDDEN,
            "the request is signed by a different device".into(),
        ));
    }
    let target = uri
        .path_and_query()
        .map_or_else(|| uri.path(), |p| p.as_str());
    let archive = state.archive.lock().expect("archive lock");
    let mut devices = state.devices.lock().expect("devices lock");
    // Authenticate before looking at the body, so an unenrolled or forged
    // caller learns nothing from how its payload parses.
    devices.authenticate(&auth, method.as_str(), target, &body, now_ms())?;
    let work: SignedWork = serde_json::from_slice(&body).map_err(|e| bad_json(&e))?;
    if work.node != node {
        return Err(ApiError(
            StatusCode::BAD_REQUEST,
            "the receipt names a different device".into(),
        ));
    }
    let known_head = archive
        .ledger
        .block(work.claim.to_height)
        .map(|block| block.header.hash());
    let (work, created) = devices.record_work(work, known_head)?;
    let status = if created {
        StatusCode::CREATED
    } else {
        StatusCode::OK
    };
    Ok((status, Json(json!(work))))
}
