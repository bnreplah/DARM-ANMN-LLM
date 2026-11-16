//! A2A Agent Cards signed with a DARM node identity.
//!
//! The Agent2Agent protocol (A2A v1.0) lets agents discover each other
//! through a JSON *Agent Card*, optionally signed: a JSON Web Signature
//! (RFC 7515) over the card canonicalized with JCS (RFC 8785), with the
//! `signatures` field left out (A2A specification, section 8.4).
//!
//! This module builds such a card, signs it with the node's Ed25519 key
//! (JWS `alg` `EdDSA`), and verifies cards signed that way. The key id
//! (`kid`) is the node id in hex, which *is* the public key, so a verifier
//! needs no key download: it checks the signature, then decides whether it
//! trusts that node id (for DARM, by looking it up on the ledger).
//!
//! What this is not: an A2A server. A card must list at least one interface
//! that speaks A2A; this module signs whatever interfaces the operator
//! declares and does not provide one. `jku` URLs are never fetched.
//!
//! A JWS signing input is base64url text containing a `.`; DARM's own signed
//! messages start with a binary length-prefixed domain tag, so a signature
//! made here can never be replayed as a ledger, memory or session signature.

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use darm_core::{DarmError, Hash32, NodeId, NodeIdentity, Result, Signature};
use serde_json::{Map, Value, json};

/// A2A protocol version these cards declare by default.
pub const A2A_VERSION: &str = "1.0";
/// Extension that binds a card to a DARM node and chain.
pub const DARM_EXTENSION_URI: &str = "urn:darm-ann:agent-identity:v1";
/// Largest card accepted for signing or verification.
pub const MAX_CARD_BYTES: usize = 64 * 1024;
/// Most signatures examined on one card.
pub const MAX_SIGNATURES: usize = 8;
const MAX_DEPTH: usize = 32;
/// Largest integer JSON and ECMAScript agree on exactly.
const MAX_SAFE_INTEGER: u64 = (1 << 53) - 1;

/// One endpoint the agent can be reached at.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Interface {
    pub url: String,
    /// `JSONRPC`, `GRPC` or `HTTP+JSON`.
    pub protocol_binding: String,
    pub protocol_version: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Skill {
    pub id: String,
    pub name: String,
    pub description: String,
    pub tags: Vec<String>,
}

/// What goes into a card.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CardSpec {
    pub name: String,
    pub description: String,
    /// The agent's own version (not the protocol version).
    pub version: String,
    pub interfaces: Vec<Interface>,
    pub skills: Vec<Skill>,
    pub streaming: bool,
    /// Organization name and URL.
    pub provider: Option<(String, String)>,
    pub documentation_url: Option<String>,
}

fn reject(message: impl Into<String>) -> DarmError {
    DarmError::Rejected(message.into())
}

fn text(field: &str, value: &str, max: usize) -> Result<Value> {
    if value.trim().is_empty() || value.len() > max {
        return Err(reject(format!(
            "card field {field} must be 1..={max} bytes"
        )));
    }
    Ok(Value::String(value.to_string()))
}

fn https_url(field: &str, value: &str) -> Result<Value> {
    let local = value.starts_with("http://localhost") || value.starts_with("http://127.0.0.1");
    if !(value.starts_with("https://") || local) || value.chars().any(char::is_whitespace) {
        return Err(reject(format!("card field {field} must be an https URL")));
    }
    text(field, value, 2_048)
}

/// Build an unsigned A2A v1.0 Agent Card bound to `node` on `chain`.
///
/// Optional fields that would hold a default (empty) value are left out, as
/// the A2A signing rules require.
pub fn build_card(spec: &CardSpec, node: &NodeId, chain: &Hash32) -> Result<Value> {
    if spec.interfaces.is_empty() || spec.interfaces.len() > 8 {
        return Err(reject(
            "an Agent Card needs 1..=8 interfaces that speak A2A; this node does not provide one itself",
        ));
    }
    if spec.skills.is_empty() || spec.skills.len() > 32 {
        return Err(reject("an Agent Card needs 1..=32 skills"));
    }
    let interfaces = spec
        .interfaces
        .iter()
        .map(|i| {
            if !["JSONRPC", "GRPC", "HTTP+JSON"].contains(&i.protocol_binding.as_str()) {
                return Err(reject("protocolBinding must be JSONRPC, GRPC or HTTP+JSON"));
            }
            Ok(json!({
                "url": https_url("supportedInterfaces.url", &i.url)?,
                "protocolBinding": i.protocol_binding,
                "protocolVersion": text("protocolVersion", &i.protocol_version, 16)?,
            }))
        })
        .collect::<Result<Vec<_>>>()?;
    let skills = spec
        .skills
        .iter()
        .map(|s| {
            if s.tags.is_empty() || s.tags.len() > 16 {
                return Err(reject("a skill needs 1..=16 tags"));
            }
            Ok(json!({
                "id": text("skill.id", &s.id, 64)?,
                "name": text("skill.name", &s.name, 128)?,
                "description": text("skill.description", &s.description, 1_024)?,
                "tags": s.tags.iter().map(|t| text("skill.tag", t, 64)).collect::<Result<Vec<_>>>()?,
            }))
        })
        .collect::<Result<Vec<_>>>()?;
    let mut card = Map::new();
    card.insert("name".into(), text("name", &spec.name, 128)?);
    card.insert(
        "description".into(),
        text("description", &spec.description, 2_048)?,
    );
    card.insert("version".into(), text("version", &spec.version, 32)?);
    card.insert("supportedInterfaces".into(), Value::Array(interfaces));
    card.insert(
        "capabilities".into(),
        json!({
            "streaming": spec.streaming,
            "pushNotifications": false,
            "extensions": [{
                "uri": DARM_EXTENSION_URI,
                "description": "Binds this card to a DARM-ANN node identity and ledger chain",
                "params": {"nodeId": node.to_hex(), "chainId": chain.to_hex()},
            }],
        }),
    );
    card.insert("defaultInputModes".into(), json!(["text/plain"]));
    card.insert("defaultOutputModes".into(), json!(["text/plain"]));
    card.insert("skills".into(), Value::Array(skills));
    if let Some((organization, url)) = &spec.provider {
        card.insert(
            "provider".into(),
            json!({
                "organization": text("provider.organization", organization, 128)?,
                "url": https_url("provider.url", url)?,
            }),
        );
    }
    if let Some(url) = &spec.documentation_url {
        card.insert(
            "documentationUrl".into(),
            https_url("documentationUrl", url)?,
        );
    }
    Ok(Value::Object(card))
}

fn write_canonical(value: &Value, out: &mut String, depth: usize) -> Result<()> {
    if depth > MAX_DEPTH {
        return Err(reject("card nests too deeply"));
    }
    match value {
        Value::Null | Value::Bool(_) | Value::String(_) => {
            // serde_json's compact form of these matches RFC 8785.
            out.push_str(&serde_json::to_string(value).map_err(|e| reject(e.to_string()))?);
        }
        Value::Number(number) => {
            // RFC 8785 formats numbers as ECMAScript does. Integers in the
            // exactly-representable range print the same everywhere; anything
            // else is refused rather than risk signing different bytes.
            let exact = number.as_u64().map(|n| n <= MAX_SAFE_INTEGER).or_else(|| {
                number
                    .as_i64()
                    .map(|n| n.unsigned_abs() <= MAX_SAFE_INTEGER)
            });
            if exact != Some(true) {
                return Err(reject("card contains a number that is not a safe integer"));
            }
            out.push_str(&number.to_string());
        }
        Value::Array(items) => {
            out.push('[');
            for (i, item) in items.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                write_canonical(item, out, depth + 1)?;
            }
            out.push(']');
        }
        Value::Object(object) => {
            // Keys sort by UTF-16 code units (RFC 8785 section 3.2.3).
            let mut keys: Vec<&String> = object.keys().collect();
            keys.sort_by(|a, b| a.encode_utf16().cmp(b.encode_utf16()));
            out.push('{');
            for (i, key) in keys.into_iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                out.push_str(&serde_json::to_string(key).map_err(|e| reject(e.to_string()))?);
                out.push(':');
                write_canonical(&object[key], out, depth + 1)?;
            }
            out.push('}');
        }
    }
    Ok(())
}

/// RFC 8785 canonical form of a JSON value (safe integers only).
pub fn canonicalize(value: &Value) -> Result<String> {
    let mut out = String::new();
    write_canonical(value, &mut out, 0)?;
    if out.len() > MAX_CARD_BYTES {
        return Err(DarmError::LimitExceeded("agent card".into()));
    }
    Ok(out)
}

/// The bytes a card signature covers: the card without `signatures`.
pub fn signing_payload(card: &Value) -> Result<String> {
    let Value::Object(object) = card else {
        return Err(reject("an Agent Card is a JSON object"));
    };
    let mut unsigned = object.clone();
    unsigned.remove("signatures");
    canonicalize(&Value::Object(unsigned))
}

/// Drop nulls, empty arrays and empty objects: values a serializer may add
/// or omit for unset optional fields without changing what the card says.
fn without_empty(value: &Value) -> Value {
    match value {
        Value::Object(object) => Value::Object(
            object
                .iter()
                .map(|(k, v)| (k.clone(), without_empty(v)))
                .filter(|(_, v)| !is_empty(v))
                .collect(),
        ),
        Value::Array(items) => Value::Array(items.iter().map(without_empty).collect()),
        other => other.clone(),
    }
}

fn is_empty(value: &Value) -> bool {
    match value {
        Value::Null => true,
        Value::Array(items) => items.is_empty(),
        Value::Object(object) => object.is_empty(),
        _ => false,
    }
}

fn signing_input(protected: &str, payload: &str) -> Vec<u8> {
    format!("{protected}.{}", URL_SAFE_NO_PAD.encode(payload)).into_bytes()
}

/// Sign `card` as `identity`, appending to its `signatures`. `jku`, if
/// given, is advertised as the URL of the key set (see [`jwks`]).
pub fn sign_card(identity: &NodeIdentity, mut card: Value, jku: Option<&str>) -> Result<Value> {
    let payload = signing_payload(&card)?;
    let mut header = Map::new();
    header.insert("alg".into(), json!("EdDSA"));
    header.insert("typ".into(), json!("JOSE"));
    header.insert("kid".into(), json!(identity.node_id().to_hex()));
    if let Some(jku) = jku {
        header.insert("jku".into(), https_url("jku", jku)?);
    }
    let protected = URL_SAFE_NO_PAD.encode(canonicalize(&Value::Object(header))?);
    let Signature::Ed25519(signature) = identity.sign(&signing_input(&protected, &payload));
    let entry = json!({
        "protected": protected,
        "signature": URL_SAFE_NO_PAD.encode(signature),
    });
    let object = card.as_object_mut().expect("signing_payload checked this");
    match object.entry("signatures").or_insert_with(|| json!([])) {
        Value::Array(list) if list.len() < MAX_SIGNATURES => list.push(entry),
        Value::Array(_) => return Err(DarmError::LimitExceeded("card signatures".into())),
        _ => return Err(reject("signatures must be an array")),
    }
    Ok(card)
}

fn verify_one(entry: &Value, payloads: &[String]) -> Option<NodeId> {
    let protected = entry.get("protected")?.as_str()?;
    let header: Value = serde_json::from_slice(&URL_SAFE_NO_PAD.decode(protected).ok()?).ok()?;
    // "Ed25519" is the fully-specified name for the same algorithm.
    if !matches!(header.get("alg")?.as_str()?, "EdDSA" | "Ed25519") {
        return None;
    }
    let node = NodeId::from_hex(header.get("kid")?.as_str()?).ok()?;
    let bytes: [u8; 64] = URL_SAFE_NO_PAD
        .decode(entry.get("signature")?.as_str()?)
        .ok()?
        .try_into()
        .ok()?;
    let signature = Signature::Ed25519(bytes);
    payloads
        .iter()
        .any(|payload| {
            node.verify(&signing_input(protected, payload), &signature)
                .is_ok()
        })
        .then_some(node)
}

/// The DARM node ids whose signatures on `card` verify, in card order.
///
/// Only signatures whose `kid` is a DARM node id are considered; others are
/// ignored, not treated as failures. An empty result means nothing here
/// vouches for the card. A valid signature proves the holder of that node
/// key signed this exact card; whether to trust the node is the caller's
/// decision.
pub fn verify_card(card: &Value) -> Result<Vec<NodeId>> {
    let exact = signing_payload(card)?;
    let mut payloads = vec![exact];
    // Tolerate serializers that add or drop empty optional fields.
    if let Ok(relaxed) = signing_payload(&without_empty(card))
        && relaxed != payloads[0]
    {
        payloads.push(relaxed);
    }
    let mut signers = Vec::new();
    if let Some(Value::Array(list)) = card.get("signatures") {
        for entry in list.iter().take(MAX_SIGNATURES) {
            if let Some(node) = verify_one(entry, &payloads)
                && !signers.contains(&node)
            {
                signers.push(node);
            }
        }
    }
    Ok(signers)
}

/// The node and chain a card claims through the DARM extension, if any.
pub fn darm_binding(card: &Value) -> Option<(NodeId, Hash32)> {
    let params = card
        .pointer("/capabilities/extensions")?
        .as_array()?
        .iter()
        .find(|e| e.get("uri").and_then(Value::as_str) == Some(DARM_EXTENSION_URI))?
        .get("params")?;
    Some((
        NodeId::from_hex(params.get("nodeId")?.as_str()?).ok()?,
        Hash32::from_hex(params.get("chainId")?.as_str()?).ok()?,
    ))
}

/// JSON Web Key Set holding `node`'s public key, for serving at a `jku`.
pub fn jwks(node: &NodeId) -> Value {
    json!({"keys": [{
        "kty": "OKP",
        "crv": "Ed25519",
        "x": URL_SAFE_NO_PAD.encode(node.as_bytes()),
        "kid": node.to_hex(),
        "alg": "EdDSA",
        "use": "sig",
    }]})
}

#[cfg(test)]
mod tests;
