//! HTTP API of the archive node.
//!
//! Reads are public unless a read token is configured. Every write needs the
//! write token (`Authorization: Bearer ...`), compared in constant time, and
//! is still fully verified cryptographically: the token keeps strangers from
//! consuming resources, signatures keep everyone honest.

use crate::archive::Archive;
use crate::devices::{self, Devices};
use axum::extract::{DefaultBodyLimit, Path, Query, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use darm_core::ledger::Block;
use darm_core::time::now_ms;
use darm_core::{DarmError, Hash32};
use darm_memory::{InsertOutcome, Origin, SearchOptions, SignedMemory, SignedTombstone};
use serde::Deserialize;
use serde_json::{Value, json};
use std::sync::{Arc, Mutex};
use tower_http::cors::{AllowOrigin, CorsLayer};

pub const MAX_BODY_BYTES: usize = 2 * 1024 * 1024;
const MAX_BLOCKS_PER_REQUEST: usize = 64;
const MAX_MEMORIES_PER_REQUEST: usize = 64;

#[derive(Clone)]
pub struct AppState {
    pub archive: Arc<Mutex<Archive>>,
    pub write_token: Option<Arc<str>>,
    pub read_token: Option<Arc<str>>,
    pub devices: Arc<Mutex<Devices>>,
    /// When set, device enrollment needs this bearer token.
    pub enroll_token: Option<Arc<str>>,
}

/// The archive's routes without any CORS policy.
#[cfg(test)]
pub fn router(state: AppState) -> Router {
    router_with_cors(state, None)
}

/// Both CORS policies of a browser-facing archive.
pub struct Cors {
    /// Read-only: `GET`, no custom headers beyond the read token.
    pub reads: CorsLayer,
    /// Device companions: `GET` and `POST` with the `X-DARM-*` signature
    /// headers. Safe because nothing there is authorised by ambient
    /// credentials: every write is signed.
    pub devices: CorsLayer,
}

/// Routes with CORS applied per route family. The device routes get their own
/// policy so allowing a companion to post a signed receipt never lets a page
/// reach the token-gated ledger and memory writes.
pub fn router_with_cors(state: AppState, cors: Option<Cors>) -> Router {
    let reads = read_routes(state.clone());
    let device_routes = devices::routes::routes(state);
    match cors {
        Some(cors) => reads
            .layer(cors.reads)
            .merge(device_routes.layer(cors.devices)),
        None => reads.merge(device_routes),
    }
}

fn read_routes(state: AppState) -> Router {
    Router::new()
        .route("/healthz", get(|| async { "ok" }))
        .route("/v1/info", get(info))
        .route("/v1/ledger/blocks", get(list_blocks).post(post_blocks))
        .route("/v1/ledger/entries/{id}/proof", get(entry_proof))
        .route("/v1/ledger/checkpoint", get(checkpoint))
        .route("/v1/proposals", get(proposals))
        .route("/v1/memories", get(inventory).post(post_memories))
        .route("/v1/memories/search", post(search))
        .route("/v1/memories/{id}", get(get_memory))
        .route("/v1/tombstones", post(post_tombstone))
        .layer(DefaultBodyLimit::max(MAX_BODY_BYTES))
        .with_state(state)
}

/// Browser access for read-only viewers such as the node monitor.
///
/// Off unless origins are configured. Only the listed origins are allowed,
/// only for `GET`, and never with credentials, so a web page can read what
/// the node already serves but cannot be used to write to it: a cross-origin
/// `POST` fails the browser's preflight. `Authorization` is allowed as a
/// request header so a viewer can present a read token the user typed in.
#[cfg(test)]
pub fn cors_layer(origins: &[String]) -> anyhow::Result<Option<CorsLayer>> {
    Ok(cors(origins)?.map(|cors| cors.reads))
}

/// Parse `--cors-origins` into both policies.
pub fn cors(origins: &[String]) -> anyhow::Result<Option<Cors>> {
    if origins.is_empty() {
        return Ok(None);
    }
    let mut allowed = Vec::with_capacity(origins.len());
    for origin in origins {
        let origin = origin.trim();
        let rest = origin
            .strip_prefix("https://")
            .or_else(|| origin.strip_prefix("http://"));
        // An origin is scheme://host[:port] with no path. A wildcard would
        // let any page on the web read through a visitor's network position.
        match rest {
            Some(host) if !host.is_empty() && !host.contains(['/', '*', ' ']) => {}
            _ => anyhow::bail!(
                "CORS origin {origin:?} must look like https://host[:port] (no path, no wildcard)"
            ),
        }
        allowed.push(header::HeaderValue::from_str(origin)?);
    }
    let max_age = std::time::Duration::from_secs(600);
    let reads = CorsLayer::new()
        .allow_origin(AllowOrigin::list(allowed.clone()))
        .allow_methods([axum::http::Method::GET])
        .allow_headers([header::AUTHORIZATION])
        .max_age(max_age);
    let devices = CorsLayer::new()
        .allow_origin(AllowOrigin::list(allowed))
        .allow_methods([axum::http::Method::GET, axum::http::Method::POST])
        .allow_headers([
            header::AUTHORIZATION,
            header::CONTENT_TYPE,
            header::HeaderName::from_static("x-darm-node"),
            header::HeaderName::from_static("x-darm-timestamp"),
            header::HeaderName::from_static("x-darm-nonce"),
            header::HeaderName::from_static("x-darm-signature"),
        ])
        .max_age(max_age);
    Ok(Some(Cors { reads, devices }))
}

pub struct ApiError(pub(crate) StatusCode, pub(crate) String);

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (self.0, Json(json!({"error": self.1}))).into_response()
    }
}

impl From<DarmError> for ApiError {
    fn from(error: DarmError) -> Self {
        let status = match &error {
            DarmError::Unauthorized(_)
            | DarmError::InvalidSignature
            | DarmError::InvalidPublicKey => StatusCode::FORBIDDEN,
            DarmError::LimitExceeded(_) => StatusCode::TOO_MANY_REQUESTS,
            DarmError::Io(_) | DarmError::Entropy(_) => StatusCode::INTERNAL_SERVER_ERROR,
            _ => StatusCode::UNPROCESSABLE_ENTITY,
        };
        Self(status, error.to_string())
    }
}

pub(crate) type ApiResult<T> = Result<T, ApiError>;

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

pub(crate) fn check_token(
    headers: &HeaderMap,
    expected: Option<&str>,
    required: bool,
) -> ApiResult<()> {
    let Some(expected) = expected else {
        return if required {
            Err(ApiError(
                StatusCode::FORBIDDEN,
                "writes are disabled on this node".into(),
            ))
        } else {
            Ok(())
        };
    };
    let presented = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .unwrap_or("");
    if constant_time_eq(presented.as_bytes(), expected.as_bytes()) {
        Ok(())
    } else {
        Err(ApiError(
            StatusCode::UNAUTHORIZED,
            "missing or invalid bearer token".into(),
        ))
    }
}

pub(crate) fn read_auth(state: &AppState, headers: &HeaderMap) -> ApiResult<()> {
    check_token(headers, state.read_token.as_deref(), false)
}

fn write_auth(state: &AppState, headers: &HeaderMap) -> ApiResult<()> {
    check_token(headers, state.write_token.as_deref(), true)
}

fn parse_id(value: &str) -> ApiResult<Hash32> {
    Hash32::from_hex(value).map_err(|e| ApiError(StatusCode::BAD_REQUEST, e.to_string()))
}

async fn info(State(state): State<AppState>, headers: HeaderMap) -> ApiResult<Json<Value>> {
    read_auth(&state, &headers)?;
    let archive = state.archive.lock().expect("archive lock");
    let ledger = &archive.ledger;
    Ok(Json(json!({
        "chain": ledger.config().name,
        "chain_id": ledger.chain_id(),
        "genesis": ledger.config().genesis_hash(),
        "validators": ledger.validators().members().map(|(n, w)| json!({"node": n, "weight": w})).collect::<Vec<_>>(),
        "genesis_validators": ledger.config().validators.members().map(|(n, w)| json!({"node": n, "weight": w})).collect::<Vec<_>>(),
        "activations": ledger.config().activations.iter().map(|(f, h)| (f.name().to_string(), json!(h))).collect::<serde_json::Map<_, _>>(),
        "activations_commitment": ledger.config().activations_commitment(),
        "height": ledger.height(),
        "head": ledger.head_hash(),
        "committed_memories": ledger.state().committed_memories(),
        "memory": archive.memory.stats(),
        "writes_enabled": state.write_token.is_some(),
        "devices": state.devices.lock().expect("devices lock").len(),
    })))
}

#[derive(Deserialize)]
struct BlockQuery {
    /// Return blocks with height greater than this.
    #[serde(default)]
    after: u64,
    #[serde(default)]
    limit: Option<usize>,
}

async fn list_blocks(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<BlockQuery>,
) -> ApiResult<Json<Vec<Block>>> {
    read_auth(&state, &headers)?;
    let limit = query.limit.unwrap_or(32).clamp(1, MAX_BLOCKS_PER_REQUEST);
    let archive = state.archive.lock().expect("archive lock");
    let blocks = archive
        .ledger
        .blocks()
        .iter()
        .skip(usize::try_from(query.after).unwrap_or(usize::MAX))
        .take(limit)
        .cloned()
        .collect();
    Ok(Json(blocks))
}

async fn post_blocks(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(blocks): Json<Vec<Block>>,
) -> ApiResult<Json<Value>> {
    write_auth(&state, &headers)?;
    if blocks.len() > MAX_BLOCKS_PER_REQUEST {
        return Err(ApiError(
            StatusCode::PAYLOAD_TOO_LARGE,
            "too many blocks".into(),
        ));
    }
    let mut archive = state.archive.lock().expect("archive lock");
    let height = archive.append_blocks(blocks, now_ms())?;
    Ok(Json(
        json!({"height": height, "head": archive.ledger.head_hash()}),
    ))
}

async fn entry_proof(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> ApiResult<Json<Value>> {
    read_auth(&state, &headers)?;
    let id = parse_id(&id)?;
    let archive = state.archive.lock().expect("archive lock");
    let ledger = &archive.ledger;
    let Some((height, proof)) = ledger.locate_entry(&id) else {
        return Err(ApiError(
            StatusCode::NOT_FOUND,
            "entry is not finalized here".into(),
        ));
    };
    let block = ledger.block(height).expect("located block exists");
    Ok(Json(json!({
        "chain_id": ledger.chain_id(),
        "height": height,
        "block_hash": block.hash(),
        "entries_root": block.header.entries_root,
        "proof": proof,
    })))
}

/// Latest head in the shape the `DarmAnchor` contract's `approveCheckpoint`
/// takes, so validators can anchor it on Ethereum.
async fn checkpoint(State(state): State<AppState>, headers: HeaderMap) -> ApiResult<Json<Value>> {
    read_auth(&state, &headers)?;
    let archive = state.archive.lock().expect("archive lock");
    let ledger = &archive.ledger;
    let Some(block) = ledger.blocks().last() else {
        return Err(ApiError(
            StatusCode::NOT_FOUND,
            "no finalized blocks yet".into(),
        ));
    };
    Ok(Json(json!({
        "chain_id": format!("0x{}", ledger.chain_id().to_hex()),
        "height": block.header.height,
        "block_hash": format!("0x{}", block.hash().to_hex()),
        "entries_root": format!("0x{}", block.header.entries_root.to_hex()),
    })))
}

#[derive(Deserialize)]
struct ProposalQuery {
    kind: Option<String>,
    outcome: Option<String>,
}

async fn proposals(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<ProposalQuery>,
) -> ApiResult<Json<Vec<Value>>> {
    read_auth(&state, &headers)?;
    let archive = state.archive.lock().expect("archive lock");
    let mut list = archive.passed_proposals(query.kind.as_deref(), now_ms());
    if let Some(outcome) = query.outcome {
        list.retain(|p| p["outcome"] == outcome.as_str());
    }
    Ok(Json(list))
}

#[derive(Deserialize)]
struct InventoryQuery {
    limit: Option<usize>,
}

async fn inventory(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<InventoryQuery>,
) -> ApiResult<Json<Vec<Hash32>>> {
    read_auth(&state, &headers)?;
    let limit = query.limit.unwrap_or(256).clamp(1, 1_024);
    Ok(Json(
        state
            .archive
            .lock()
            .expect("archive lock")
            .memory
            .inventory(limit),
    ))
}

async fn get_memory(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> ApiResult<Json<SignedMemory>> {
    read_auth(&state, &headers)?;
    let id = parse_id(&id)?;
    let archive = state.archive.lock().expect("archive lock");
    archive
        .memory
        .export_shareable(&[id])
        .pop()
        .map(Json)
        .ok_or_else(|| ApiError(StatusCode::NOT_FOUND, "unknown memory".into()))
}

async fn post_memories(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(memories): Json<Vec<SignedMemory>>,
) -> ApiResult<Json<Vec<Value>>> {
    write_auth(&state, &headers)?;
    if memories.len() > MAX_MEMORIES_PER_REQUEST {
        return Err(ApiError(
            StatusCode::PAYLOAD_TOO_LARGE,
            "too many memories".into(),
        ));
    }
    let mut archive = state.archive.lock().expect("archive lock");
    let now = now_ms();
    let results = memories
        .into_iter()
        .map(|memory| {
            let id = memory.id();
            let finalized = archive.ledger.state().is_committed(&id);
            match archive.memory.insert(memory, Origin::Peer, now) {
                Ok(outcome) => {
                    if finalized {
                        let _ = archive.memory.mark_finalized(&id, now);
                    }
                    json!({"id": id, "stored": outcome == InsertOutcome::Inserted, "finalized": finalized})
                }
                Err(error) => json!({"id": id, "error": error.to_string()}),
            }
        })
        .collect();
    Ok(Json(results))
}

#[derive(Deserialize)]
struct SearchBody {
    query: String,
    #[serde(default)]
    limit: Option<usize>,
    /// Only ledger-finalized memories (default true).
    #[serde(default)]
    finalized_only: Option<bool>,
}

async fn search(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<SearchBody>,
) -> ApiResult<Json<Value>> {
    read_auth(&state, &headers)?;
    if body.query.len() > 4_096 {
        return Err(ApiError(
            StatusCode::PAYLOAD_TOO_LARGE,
            "query too long".into(),
        ));
    }
    let options = SearchOptions {
        limit: body.limit.unwrap_or(10).clamp(1, 50),
        require_finalized_from_peers: body.finalized_only.unwrap_or(true),
        ..SearchOptions::default()
    };
    let hits =
        state
            .archive
            .lock()
            .expect("archive lock")
            .memory
            .search(&body.query, &options, now_ms());
    Ok(Json(json!({"hits": hits})))
}

async fn post_tombstone(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(tombstone): Json<SignedTombstone>,
) -> ApiResult<Json<Value>> {
    write_auth(&state, &headers)?;
    let applied = state
        .archive
        .lock()
        .expect("archive lock")
        .memory
        .apply_tombstone(tombstone)?;
    Ok(Json(json!({"applied": applied})))
}

#[cfg(test)]
pub(crate) mod tests_support;

#[cfg(test)]
mod tests;
