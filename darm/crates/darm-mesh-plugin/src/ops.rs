//! Operations the host projects as MCP tools and plugin HTTP endpoints.
//!
//! These run with the authority of the local node operator (the host's
//! management surface), so they may write memories as this node.

use crate::SharedNode;
use crate::node::namespace_tag;
use crate::send_outbound;
use darm_core::Hash32;
use darm_core::time::now_ms;
use darm_memory::{MemoryKind, MemoryRecord, Scope};
use mesh_llm_plugin::{
    ManifestEntry, OperationRouter, PluginError, json_schema_operation, operation,
};
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::{Value, json};
use std::sync::Arc;

#[derive(Debug, Deserialize, JsonSchema, Default)]
pub struct StatusArgs {}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct SearchArgs {
    /// Natural-language query.
    pub query: String,
    /// Memory namespace (default "default").
    #[serde(default)]
    pub namespace: Option<String>,
    /// Maximum hits, 1..=20 (default 5).
    #[serde(default)]
    pub limit: Option<usize>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct StoreArgs {
    pub content: String,
    /// episodic | semantic | procedural | affective (default semantic).
    #[serde(default)]
    pub kind: Option<MemoryKindArg>,
    /// 0.0..=1.0 (default 0.5).
    #[serde(default)]
    pub importance: Option<f32>,
    /// -1.0..=1.0 affect (default 0).
    #[serde(default)]
    pub valence: Option<f32>,
    #[serde(default)]
    pub tags: Vec<String>,
    #[serde(default)]
    pub namespace: Option<String>,
    /// Share with the mesh (only honoured when the node enables sharing).
    #[serde(default)]
    pub share: bool,
}

#[derive(Debug, Deserialize, JsonSchema, Clone, Copy)]
#[serde(rename_all = "snake_case")]
pub enum MemoryKindArg {
    Episodic,
    Semantic,
    Procedural,
    Affective,
}

impl From<MemoryKindArg> for MemoryKind {
    fn from(value: MemoryKindArg) -> Self {
        match value {
            MemoryKindArg::Episodic => Self::Episodic,
            MemoryKindArg::Semantic => Self::Semantic,
            MemoryKindArg::Procedural => Self::Procedural,
            MemoryKindArg::Affective => Self::Affective,
        }
    }
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct IdArgs {
    /// 64-hex-character id.
    pub id: String,
}

const DARM_STATUS_DESC: &str = "DARM-ANN node, ledger, memory and swarm status";
const DARM_MEMORY_SEARCH_DESC: &str = "Associative recall over the distributed memory";
const DARM_MEMORY_STORE_DESC: &str = "Store a signed memory as this node";
const DARM_MEMORY_FORGET_DESC: &str =
    "Forget one of this node's memories (tombstone + ledger revoke)";
const DARM_LEDGER_PROOF_DESC: &str =
    "Merkle inclusion proof for a finalized ledger entry (for light clients and on-chain anchors)";

/// Manifest declarations; the host projects only declared operations as MCP
/// tools and `POST /api/plugins/darm/tools/<name>`.
pub fn manifest_entries() -> Vec<ManifestEntry> {
    vec![
        operation::<StatusArgs>("darm_status", DARM_STATUS_DESC).into(),
        operation::<SearchArgs>("darm_memory_search", DARM_MEMORY_SEARCH_DESC).into(),
        operation::<StoreArgs>("darm_memory_store", DARM_MEMORY_STORE_DESC).into(),
        operation::<IdArgs>("darm_memory_forget", DARM_MEMORY_FORGET_DESC).into(),
        operation::<IdArgs>("darm_ledger_proof", DARM_LEDGER_PROOF_DESC).into(),
    ]
}

fn bad(error: impl std::fmt::Display) -> PluginError {
    PluginError::invalid_params(error.to_string())
}

pub fn router(node: SharedNode) -> OperationRouter {
    let mut router = OperationRouter::new();

    let n = Arc::clone(&node);
    router.add_json(
        json_schema_operation::<StatusArgs>("darm_status", DARM_STATUS_DESC),
        move |_args: StatusArgs, _context| {
            let n = Arc::clone(&n);
            Box::pin(async move { Ok(n.lock().expect("darm node lock").status(now_ms())) })
        },
    );

    let n = Arc::clone(&node);
    router.add_json(
        json_schema_operation::<SearchArgs>("darm_memory_search", DARM_MEMORY_SEARCH_DESC),
        move |args: SearchArgs, _context| {
            let n = Arc::clone(&n);
            Box::pin(async move {
                let namespace = args.namespace.unwrap_or_else(|| "default".into());
                let limit = args.limit.unwrap_or(5).clamp(1, 20);
                let hits = n.lock().expect("darm node lock").recall(
                    &args.query,
                    &namespace,
                    limit,
                    now_ms(),
                );
                let hits: Vec<Value> = hits
                    .into_iter()
                    .map(|hit| {
                        json!({
                            "id": hit.id,
                            "score": hit.score,
                            "similarity": hit.similarity,
                            "tier": hit.tier,
                            "finalized": hit.finalized,
                            "author": hit.record.author,
                            "kind": hit.record.kind,
                            "scope": hit.record.scope,
                            "tags": hit.record.tags,
                            "created_ms": hit.record.created_ms,
                            "content": hit.record.content,
                        })
                    })
                    .collect();
                Ok(json!({ "hits": hits }))
            })
        },
    );

    let n = Arc::clone(&node);
    router.add_json(
        json_schema_operation::<StoreArgs>("darm_memory_store", DARM_MEMORY_STORE_DESC),
        move |args: StoreArgs, context| {
            let n = Arc::clone(&n);
            Box::pin(async move {
                let importance = args.importance.unwrap_or(0.5);
                let valence = args.valence.unwrap_or(0.0);
                if !(0.0..=1.0).contains(&importance) || !(-1.0..=1.0).contains(&valence) {
                    return Err(bad("importance must be 0..=1 and valence -1..=1"));
                }
                let namespace = args.namespace.unwrap_or_else(|| "default".into());
                let (signed, outbound) = {
                    let mut node = n.lock().expect("darm node lock");
                    let scope = if args.share && node.config.share {
                        Scope::Mesh
                    } else {
                        Scope::Local
                    };
                    let record = MemoryRecord {
                        author: node.node_id(),
                        created_ms: now_ms(),
                        kind: args.kind.map_or(MemoryKind::Semantic, Into::into),
                        scope,
                        importance_milli: (importance * 1_000.0).round() as u16,
                        valence_milli: (valence * 1_000.0).round() as i16,
                        tags: args.tags,
                        content: args.content,
                        parent: None,
                    };
                    node.remember(record, &namespace, now_ms()).map_err(bad)?
                };
                send_outbound(context, outbound)
                    .await
                    .map_err(|e| PluginError::internal(e.to_string()))?;
                Ok(json!({
                    "id": signed.id(),
                    "scope": signed.record.scope,
                    "namespace_tag": namespace_tag(&namespace),
                }))
            })
        },
    );

    let n = Arc::clone(&node);
    router.add_json(
        json_schema_operation::<IdArgs>("darm_memory_forget", DARM_MEMORY_FORGET_DESC),
        move |args: IdArgs, context| {
            let n = Arc::clone(&n);
            Box::pin(async move {
                let id = Hash32::from_hex(&args.id).map_err(bad)?;
                let outbound = n
                    .lock()
                    .expect("darm node lock")
                    .forget(&id, now_ms())
                    .map_err(bad)?;
                send_outbound(context, outbound)
                    .await
                    .map_err(|e| PluginError::internal(e.to_string()))?;
                Ok(json!({ "forgotten": id }))
            })
        },
    );

    let n = Arc::clone(&node);
    router.add_json(
        json_schema_operation::<IdArgs>("darm_ledger_proof", DARM_LEDGER_PROOF_DESC),
        move |args: IdArgs, _context| {
            let n = Arc::clone(&n);
            Box::pin(async move {
                let id = Hash32::from_hex(&args.id).map_err(bad)?;
                let node = n.lock().expect("darm node lock");
                let ledger = node.ledger();
                let Some((height, proof)) = ledger.locate_entry(&id) else {
                    return Err(bad("entry is not finalized on this node's ledger"));
                };
                let block = ledger.block(height).expect("located block exists");
                Ok(json!({
                    "chain_id": ledger.chain_id(),
                    "height": height,
                    "block_hash": block.hash(),
                    "entries_root": block.header.entries_root,
                    "proof": proof,
                }))
            })
        },
    );

    router
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn router_exposes_all_operations() {
        let node = crate::node::DarmNode::ephemeral(
            crate::config::DarmConfig::default(),
            darm_core::NodeIdentity::from_secret_bytes(&[1; 32]),
            0,
        )
        .unwrap();
        let router = router(Arc::new(std::sync::Mutex::new(node)));
        let names: Vec<String> = router
            .list_tools_result()
            .tools
            .into_iter()
            .map(|t| t.name.to_string())
            .collect();
        for expected in [
            "darm_status",
            "darm_memory_search",
            "darm_memory_store",
            "darm_memory_forget",
            "darm_ledger_proof",
        ] {
            assert!(names.iter().any(|n| n == expected), "missing {expected}");
        }
        let declared: Vec<String> = manifest_entries()
            .into_iter()
            .filter_map(|entry| match entry {
                ManifestEntry::Operation(op) => Some(op.name),
                _ => None,
            })
            .collect();
        assert_eq!(
            declared.len(),
            names.len(),
            "every routed operation is declared"
        );
    }
}
