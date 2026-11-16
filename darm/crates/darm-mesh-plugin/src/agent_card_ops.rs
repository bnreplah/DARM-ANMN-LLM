//! A2A Agent Card operations.
//!
//! `darm_agent_card` builds an A2A v1.0 Agent Card for this node and signs
//! it with the node's ledger identity; the operator serves it at
//! `/.well-known/agent-card.json` beside the A2A endpoint they run (this
//! plugin does not speak A2A itself). `darm_agent_card_verify` checks
//! another agent's card and reports what this node's ledger knows about
//! whoever signed it.

use crate::SharedNode;
use crate::node::DarmNode;
use darm_protocol::agent_card::{
    A2A_VERSION, CardSpec, Interface, Skill, build_card, darm_binding, jwks, sign_card, verify_card,
};
use mesh_llm_plugin::{
    ManifestEntry, OperationRouter, PluginError, json_schema_operation, operation,
};
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::{Value, json};
use std::sync::Arc;

#[derive(Debug, Deserialize, JsonSchema)]
pub struct InterfaceArg {
    /// https URL of an endpoint that speaks A2A for this agent.
    pub url: String,
    /// `JSONRPC` (default), `GRPC` or `HTTP+JSON`.
    #[serde(default)]
    pub protocol_binding: Option<String>,
    /// A2A protocol version served there (default 1.0).
    #[serde(default)]
    pub protocol_version: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct AgentCardArgs {
    /// Where this agent can be reached over A2A. At least one.
    pub interfaces: Vec<InterfaceArg>,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub provider_organization: Option<String>,
    #[serde(default)]
    pub provider_url: Option<String>,
    #[serde(default)]
    pub documentation_url: Option<String>,
    /// URL where you will serve the returned key set, advertised as `jku`.
    #[serde(default)]
    pub jwks_url: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct VerifyCardArgs {
    /// The Agent Card JSON as fetched.
    pub card: Value,
}

const CARD_DESC: &str = "Build an A2A Agent Card for this node, signed with its ledger identity, to serve at /.well-known/agent-card.json";
const VERIFY_DESC: &str =
    "Verify an A2A Agent Card's DARM signatures and report what the ledger knows about the signer";

pub fn manifest_entries() -> Vec<ManifestEntry> {
    vec![
        operation::<AgentCardArgs>("darm_agent_card", CARD_DESC).into(),
        operation::<VerifyCardArgs>("darm_agent_card_verify", VERIFY_DESC).into(),
    ]
}

fn bad(error: impl std::fmt::Display) -> PluginError {
    PluginError::invalid_params(error.to_string())
}

pub fn card(node: &DarmNode, args: AgentCardArgs) -> Result<Value, PluginError> {
    let provider = match (args.provider_organization, args.provider_url) {
        (Some(organization), Some(url)) => Some((organization, url)),
        (None, None) => None,
        _ => {
            return Err(bad(
                "give both provider_organization and provider_url, or neither",
            ));
        }
    };
    let spec = CardSpec {
        name: args
            .name
            .unwrap_or_else(|| format!("DARM-ANN node {}", node.node_id().short())),
        description: args.description.unwrap_or_else(|| {
            "Chat backed by a distributed, signed memory and swarm-routed models (DARM-ANN)".into()
        }),
        version: env!("CARGO_PKG_VERSION").into(),
        interfaces: args
            .interfaces
            .into_iter()
            .map(|i| Interface {
                url: i.url,
                protocol_binding: i.protocol_binding.unwrap_or_else(|| "JSONRPC".into()),
                protocol_version: i.protocol_version.unwrap_or_else(|| A2A_VERSION.into()),
            })
            .collect(),
        skills: vec![Skill {
            id: "darm-chat".into(),
            name: "Memory-augmented chat".into(),
            description:
                "Answers using memories recalled from the mesh, routed to a suitable model".into(),
            tags: vec!["chat".into(), "memory".into(), "retrieval".into()],
        }],
        streaming: false,
        provider,
        documentation_url: args.documentation_url,
    };
    let chain = node.ledger().chain_id();
    let unsigned = build_card(&spec, &node.node_id(), &chain).map_err(bad)?;
    let signed = sign_card(node.identity(), unsigned, args.jwks_url.as_deref()).map_err(bad)?;
    Ok(json!({
        "card": signed,
        "jwks": jwks(&node.node_id()),
        "serve": {
            "card_at": "/.well-known/agent-card.json",
            "content_type": "application/a2a+json",
            "note": "Serve the card exactly as returned (any JSON formatting). Editing a field invalidates the signature; run this again instead.",
        },
    }))
}

pub fn verify(node: &DarmNode, args: VerifyCardArgs) -> Result<Value, PluginError> {
    let signers = verify_card(&args.card).map_err(bad)?;
    let ledger = node.ledger();
    let chain = ledger.chain_id();
    let binding = darm_binding(&args.card);
    let mut vouched = false;
    let described: Vec<Value> = signers
        .iter()
        .map(|signer| {
            let registration = ledger.state().registration(signer);
            let validator = ledger.is_validator(signer);
            let bound = binding.is_some_and(|(n, c)| n == *signer && c == chain);
            if bound && (validator || registration.is_some()) {
                vouched = true;
            }
            json!({
                "node": signer,
                "validator": validator,
                "registered": registration.is_some(),
                "role": registration.map(|r| r.role.clone()),
                "reputation_milli": ledger.state().reputation(signer),
                "equivocations": ledger.state().equivocations(signer),
                "card_names_this_node_on_this_chain": bound,
            })
        })
        .collect();
    Ok(json!({
        "valid_darm_signatures": described.len(),
        "signers": described,
        "claimed": binding.map(|(n, c)| json!({"node": n, "chain": c, "this_chain": c == chain})),
        "vouched_by_ledger": vouched,
        "meaning": if vouched {
            "Signed by a node registered on this chain, and the card names that node. This proves who published the card, not that the agent behaves well."
        } else if described.is_empty() {
            "No valid DARM signature. The card may be signed with other keys this node cannot judge."
        } else {
            "Validly signed, but not by a node this chain knows, or the card does not name its signer and this chain."
        },
    }))
}

pub fn register(router: &mut OperationRouter, node: &SharedNode) {
    let n = Arc::clone(node);
    router.add_json(
        json_schema_operation::<AgentCardArgs>("darm_agent_card", CARD_DESC),
        move |args: AgentCardArgs, _context| {
            let n = Arc::clone(&n);
            Box::pin(async move { card(&n.lock().expect("darm node lock"), args) })
        },
    );
    let n = Arc::clone(node);
    router.add_json(
        json_schema_operation::<VerifyCardArgs>("darm_agent_card_verify", VERIFY_DESC),
        move |args: VerifyCardArgs, _context| {
            let n = Arc::clone(&n);
            Box::pin(async move { verify(&n.lock().expect("darm node lock"), args) })
        },
    );
}
