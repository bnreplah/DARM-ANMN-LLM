//! The `darm` virtual model turn.
//!
//! 1. **Recall**: the latest user message queries the distributed memory;
//!    trusted hits are rendered into a fenced, injection-hardened system
//!    message.
//! 2. **Route**: concrete mesh models are ranked by pheromone trails
//!    (ant-colony optimization), weighted by each endpoint's CPU/GPU
//!    capacity from the compute pool, and tried in order with fallback.
//! 3. **Learn**: every attempt lays trail or danger pheromone and feeds the
//!    node's lifecycle; a successful exchange is remembered (locally unless
//!    sharing is enabled) and, when mesh-scoped, committed to the ledger.
//!
//! Callers can steer a turn with an optional `darm` object in the request,
//! which is removed before the request reaches any concrete model:
//! `{"darm": {"recall": true, "remember": true, "namespace": "team-a", "top_k": 5, "tier": "tiny"}}`.

use crate::node::DarmNode;
use crate::wire::Outbound;
use darm_core::time::now_ms;
use darm_memory::render_context;
use darm_swarm::RouteKey;
use mesh_llm_plugin::{
    HostInferenceRequest, HostInferenceResponse, VirtualModelCandidate, VirtualModelInvocation,
    VirtualModelResponse,
};
use serde_json::{Value, json};
use std::future::Future;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

pub const VIRTUAL_MODEL: &str = "darm";
const TURN_BUDGET: Duration = Duration::from_secs(120);
static TURN_SEED: AtomicU64 = AtomicU64::new(0);

/// Model tier hint (Paper 3: TinyLM 0.5–3B, Mid-Scale 7–30B, Consolidator
/// 70B+). Routing to the smallest capable tier saves compute; the hint
/// narrows candidates by advertised parameter count when any match and is
/// ignored otherwise, so it can never make a turn unservable.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tier {
    Tiny,
    Mid,
    Consolidator,
}

impl Tier {
    fn parse(value: &str) -> Option<Self> {
        Some(match value {
            "tiny" => Self::Tiny,
            "mid" => Self::Mid,
            "consolidator" => Self::Consolidator,
            _ => return None,
        })
    }

    fn contains(self, billions: f64) -> bool {
        match self {
            Self::Tiny => billions <= 3.5,
            Self::Mid => billions > 3.5 && billions <= 35.0,
            Self::Consolidator => billions > 35.0,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct TurnOptions {
    pub recall: bool,
    pub remember: bool,
    pub namespace: String,
    pub top_k: usize,
    pub tier: Option<Tier>,
}

/// Remove and parse the `darm` request extension.
pub fn take_options(
    request: &mut Value,
    remember_default: bool,
    top_k_default: usize,
) -> Result<TurnOptions, String> {
    let extension = request
        .as_object_mut()
        .and_then(|object| object.remove("darm"))
        .unwrap_or(Value::Null);
    let get_bool = |key: &str, default: bool| -> Result<bool, String> {
        match extension.get(key) {
            None | Some(Value::Null) => Ok(default),
            Some(Value::Bool(value)) => Ok(*value),
            Some(_) => Err(format!("darm.{key} must be a boolean")),
        }
    };
    if !(extension.is_null() || extension.is_object()) {
        return Err("`darm` must be an object".into());
    }
    let namespace = match extension.get("namespace") {
        None | Some(Value::Null) => "default".to_string(),
        Some(Value::String(ns)) if !ns.is_empty() && ns.len() <= 40 => ns.clone(),
        Some(_) => return Err("darm.namespace must be a 1..=40 character string".into()),
    };
    let top_k = match extension.get("top_k") {
        None | Some(Value::Null) => top_k_default,
        Some(value) => value
            .as_u64()
            .filter(|k| *k <= 20)
            .ok_or("darm.top_k must be an integer in 0..=20")? as usize,
    };
    let tier = match extension.get("tier") {
        None | Some(Value::Null) => None,
        Some(Value::String(t)) if t == "auto" => None,
        Some(Value::String(t)) => {
            Some(Tier::parse(t).ok_or("darm.tier must be auto|tiny|mid|consolidator")?)
        }
        Some(_) => return Err("darm.tier must be a string".into()),
    };
    Ok(TurnOptions {
        recall: get_bool("recall", true)?,
        remember: get_bool("remember", remember_default)?,
        namespace,
        top_k,
        tier,
    })
}

fn content_text(content: &Value) -> Option<String> {
    match content {
        Value::String(text) => Some(text.clone()),
        Value::Array(parts) => {
            let text: Vec<&str> = parts
                .iter()
                .filter_map(|part| part.get("text").and_then(Value::as_str))
                .collect();
            (!text.is_empty()).then(|| text.join("\n"))
        }
        _ => None,
    }
}

pub fn last_user_text(request: &Value) -> Option<String> {
    request
        .get("messages")?
        .as_array()?
        .iter()
        .rev()
        .find(|m| m.get("role").and_then(Value::as_str) == Some("user"))
        .and_then(|m| m.get("content"))
        .and_then(content_text)
}

/// Insert the memory block after any leading system messages so the
/// operator's own system prompt keeps precedence.
pub fn inject_memory(request: &mut Value, context: String) {
    let Some(messages) = request.get_mut("messages").and_then(Value::as_array_mut) else {
        return;
    };
    let position = messages
        .iter()
        .take_while(|m| m.get("role").and_then(Value::as_str) == Some("system"))
        .count();
    messages.insert(position, json!({"role": "system", "content": context}));
}

fn assistant_text(body: &Value) -> Option<String> {
    body.pointer("/choices/0/message/content")
        .and_then(content_text)
        .filter(|text| !text.trim().is_empty())
}

fn contains_any_key(value: &Value, keys: &[&str]) -> bool {
    match value {
        Value::Object(object) => object
            .iter()
            .any(|(key, value)| keys.contains(&key.as_str()) || contains_any_key(value, keys)),
        Value::Array(values) => values.iter().any(|value| contains_any_key(value, keys)),
        _ => false,
    }
}

/// Drop candidates that cannot serve the request's modalities; prefer
/// tool-capable and non-deprioritized ones without emptying the pool.
fn eligible(
    request: &Value,
    candidates: Vec<VirtualModelCandidate>,
    tier: Option<Tier>,
) -> Vec<VirtualModelCandidate> {
    let needs_vision = contains_any_key(request, &["image_url", "input_image"]);
    let needs_audio = contains_any_key(request, &["input_audio", "audio_url"]);
    let needs_tools = request
        .get("tools")
        .and_then(Value::as_array)
        .is_some_and(|tools| !tools.is_empty());
    let mut pool: Vec<_> = candidates
        .into_iter()
        .filter(|c| (!needs_vision || c.supports_vision) && (!needs_audio || c.supports_audio))
        .collect();
    if needs_tools && pool.iter().any(|c| c.supports_tools) {
        pool.retain(|c| c.supports_tools);
    }
    if pool.iter().any(|c| !c.deprioritized) {
        pool.retain(|c| !c.deprioritized);
    }
    if let Some(tier) = tier {
        let in_tier =
            |c: &VirtualModelCandidate| c.parameter_count_b.is_some_and(|b| tier.contains(b));
        if pool.iter().any(in_tier) {
            pool.retain(in_tier);
        }
    }
    pool
}

fn route_key(candidate: &VirtualModelCandidate) -> RouteKey {
    RouteKey {
        node: candidate.target_node_id.clone().unwrap_or_default(),
        model: candidate.model_id.clone(),
    }
}

pub fn error_response(status_code: u16, message: &str) -> VirtualModelResponse {
    VirtualModelResponse {
        status_code,
        body: json!({"error": {"message": message, "type": "darm_error"}}),
        headers: Vec::new(),
        event_stream: false,
    }
}

type SharedNode = Arc<Mutex<DarmNode>>;

fn lock(node: &SharedNode) -> std::sync::MutexGuard<'_, DarmNode> {
    node.lock().expect("darm node lock")
}

/// What a turn prepared before routing.
struct Prepared {
    options: TurnOptions,
    requested_stream: bool,
    budget_chars: usize,
}

/// A route that produced a final (non-retryable) response.
struct Routed {
    route: RouteKey,
    response: HostInferenceResponse,
    ok: bool,
}

/// Validate the request and strip the `darm` and `stream` fields.
fn prepare(request: &mut Value, node: &SharedNode) -> Result<Prepared, VirtualModelResponse> {
    match request.get("messages") {
        Some(Value::Array(messages)) if !messages.is_empty() => {}
        _ => {
            return Err(error_response(
                400,
                "darm requires a non-empty `messages` array",
            ));
        }
    }
    let (remember_default, top_k_default, budget_chars) = {
        let node = lock(node);
        (
            node.config.remember,
            node.config.recall_top_k,
            node.config.recall_budget_chars,
        )
    };
    let options = take_options(request, remember_default, top_k_default)
        .map_err(|message| error_response(400, &message))?;
    let requested_stream = request
        .as_object_mut()
        .and_then(|o| o.remove("stream"))
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    Ok(Prepared {
        options,
        requested_stream,
        budget_chars,
    })
}

/// Phase 1: inject recalled memories. Returns the number injected.
fn recall_into(
    request: &mut Value,
    query: Option<&str>,
    node: &SharedNode,
    prepared: &Prepared,
) -> usize {
    let options = &prepared.options;
    let Some(query) = query.filter(|_| options.recall && options.top_k > 0) else {
        return 0;
    };
    let hits = lock(node).recall(query, &options.namespace, options.top_k, now_ms());
    if let Some(context) = render_context(&hits, prepared.budget_chars) {
        inject_memory(request, context);
    }
    hits.len()
}

/// Phase 2a: order candidate routes by pheromone desirability.
///
/// The heuristic combines model size with the serving endpoint's place in
/// the compute pool: faster, idler hardware scores higher, and phones on
/// battery score lower so the mesh spares them.
fn rank(node: &SharedNode, candidates: &[VirtualModelCandidate]) -> Vec<RouteKey> {
    let now = now_ms();
    let node = lock(node);
    let routes: Vec<(RouteKey, f64)> = candidates
        .iter()
        .map(|c| {
            let size = c.parameter_count_b.map_or(1.0, |b| b.max(0.5).sqrt());
            let compute = c
                .target_node_id
                .as_deref()
                .map_or(1.0, |peer| node.route_factor(peer, now));
            (route_key(c), size * compute)
        })
        .collect();
    let seed = now ^ TURN_SEED.fetch_add(1, Ordering::Relaxed).rotate_left(32);
    node.rank_routes(&routes, now, seed)
}

/// Phase 2b: try routes in order, laying pheromone for every attempt.
async fn route_turn<F, Fut>(
    request: &Value,
    order: Vec<RouteKey>,
    node: &SharedNode,
    infer: &mut F,
) -> Result<Routed, VirtualModelResponse>
where
    F: FnMut(HostInferenceRequest) -> Fut,
    Fut: Future<Output = anyhow::Result<HostInferenceResponse>>,
{
    let deadline = Instant::now() + TURN_BUDGET;
    let mut last_failure = error_response(504, "darm turn timed out");
    for route in order {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            break;
        }
        let started = Instant::now();
        let attempt = HostInferenceRequest {
            model_id: route.model.clone(),
            target_node_id: (!route.node.is_empty()).then(|| route.node.clone()),
            request: request.clone(),
            timeout_ms: Some(remaining.as_millis().min(u128::from(u64::MAX)) as u64),
        };
        let outcome = tokio::time::timeout(remaining, infer(attempt)).await;
        let latency_ms = started.elapsed().as_secs_f64() * 1_000.0;
        let response = match outcome {
            Ok(Ok(response)) => response,
            Ok(Err(error)) => {
                lock(node).record_outcome(&route, false, latency_ms, now_ms());
                last_failure =
                    error_response(502, &format!("route {} failed: {error}", route.model));
                continue;
            }
            Err(_) => {
                lock(node).record_outcome(&route, false, latency_ms, now_ms());
                break;
            }
        };
        let ok = (200..300).contains(&response.status_code) && response.body.get("error").is_none();
        lock(node).record_outcome(&route, ok, latency_ms, now_ms());
        if matches!(response.status_code, 502..=504) {
            last_failure = VirtualModelResponse {
                status_code: response.status_code,
                body: response.body,
                headers: Vec::new(),
                event_stream: false,
            };
            continue;
        }
        return Ok(Routed {
            route,
            response,
            ok,
        });
    }
    Err(last_failure)
}

/// Phase 3: remember a successful exchange and frame the response.
fn learn(
    node: &SharedNode,
    routed: Routed,
    query: Option<&str>,
    recalled: usize,
    prepared: &Prepared,
) -> (VirtualModelResponse, Vec<Outbound>) {
    let Routed {
        route,
        response,
        ok,
    } = routed;
    let mut node = lock(node);
    let mut headers = vec![
        ("x-darm-memories".to_string(), recalled.to_string()),
        ("x-darm-route".to_string(), route.model.clone()),
        ("x-darm-node".to_string(), node.node_id().short()),
    ];
    if let Some(served_by) = &response.served_by {
        headers.push(("x-mesh-served-by".into(), served_by.clone()));
    }
    let mut outbound = Vec::new();
    let options = &prepared.options;
    if ok
        && options.remember
        && let (Some(user), Some(answer)) = (query, assistant_text(&response.body))
    {
        match node.remember_exchange(user, &answer, &route.model, &options.namespace, now_ms()) {
            Ok((id, out)) => {
                headers.push(("x-darm-memory-id".into(), id.to_hex()));
                outbound = out;
            }
            Err(error) => tracing::debug!(%error, "darm could not remember exchange"),
        }
    }
    let mut body = response.body;
    if body.is_object() {
        body["model"] = Value::String(VIRTUAL_MODEL.into());
    }
    (
        VirtualModelResponse {
            status_code: response.status_code,
            body,
            headers,
            event_stream: prepared.requested_stream,
        },
        outbound,
    )
}

/// Run one turn. `infer` performs host inference (injected for tests).
pub async fn handle<F, Fut>(
    mut invocation: VirtualModelInvocation,
    node: SharedNode,
    mut infer: F,
) -> (VirtualModelResponse, Vec<Outbound>)
where
    F: FnMut(HostInferenceRequest) -> Fut,
    Fut: Future<Output = anyhow::Result<HostInferenceResponse>>,
{
    let prepared = match prepare(&mut invocation.request, &node) {
        Ok(prepared) => prepared,
        Err(response) => return (response, Vec::new()),
    };
    let query = last_user_text(&invocation.request);
    let recalled = recall_into(&mut invocation.request, query.as_deref(), &node, &prepared);
    let candidates = eligible(
        &invocation.request,
        std::mem::take(&mut invocation.candidates),
        prepared.options.tier,
    );
    if candidates.is_empty() {
        return (
            error_response(503, "no concrete model can serve this request"),
            Vec::new(),
        );
    }
    let order = rank(&node, &candidates);
    lock(&node).begin_request();
    let routed = route_turn(&invocation.request, order, &node, &mut infer).await;
    {
        let mut guard = lock(&node);
        guard.end_request();
        guard.counters.turns += 1;
        if routed.is_err() {
            guard.counters.turns_failed += 1;
        }
    }
    match routed {
        Ok(routed) => learn(&node, routed, query.as_deref(), recalled, &prepared),
        Err(mut failure) => {
            failure.event_stream = prepared.requested_stream && failure.status_code < 400;
            (failure, Vec::new())
        }
    }
}

#[cfg(test)]
mod tests;
