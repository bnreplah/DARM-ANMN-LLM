//! DARM-ANN-LLM: the built-in mesh-llm plugin that fuses DARM-ANN into the
//! mesh.
//!
//! What it adds to every mesh-llm node:
//!
//! - the **`darm` virtual model**: memory-augmented, pheromone-routed chat
//!   over the mesh's concrete models (see [`chat`]);
//! - a **distributed memory** of signed, content-addressed records with
//!   short/long-term consolidation ([`darm_memory`]);
//! - a **memory ledger** whose blocks finalize by validator quorum
//!   ([`darm_core::ledger`]), gossiped on the `darm.v1` mesh channel;
//! - the **biomimetic swarm** lifecycle, genome and governed replication
//!   proposals ([`darm_swarm`]);
//! - **operations** projected by the host as MCP tools and plugin HTTP:
//!   `darm_status`, `darm_memory_search`, `darm_memory_store`,
//!   `darm_memory_forget`, `darm_ledger_proof`.

pub mod archive;
pub mod chat;
pub mod config;
pub mod node;
pub mod ops;
pub mod wire;

use crate::config::DarmConfig;
use crate::node::DarmNode;
use crate::wire::{CHANNEL, Outbound, Wire};
use darm_core::time::now_ms;
use mesh_llm_plugin::proto::mesh_event::Kind as MeshEventKind;
use mesh_llm_plugin::{
    MeshVisibility, PluginContext, PluginMetadata, PluginRuntime, SimplePlugin,
    VirtualModelInvocation, VirtualModelRouter, mesh_channel, mesh_event_peer_up,
    operation_with_schema, plugin_server_info, structured_tool_result, virtual_model,
};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

pub const PLUGIN_ID: &str = "darm";
pub const HANDLER: &str = "chat";
const TICK_INTERVAL: Duration = Duration::from_secs(2);

pub const PROGRESS_LINES: [&str; 4] = [
    "Recalling distributed memory…",
    "Following pheromone trails through the mesh…",
    "Waiting on a peer model…",
    "Still working on it…",
];

pub type SharedNode = Arc<Mutex<DarmNode>>;

/// Entry point used by the mesh-llm host for the in-process built-in.
pub async fn run(stream: mesh_llm_plugin::LocalStream) -> anyhow::Result<()> {
    let config = DarmConfig::from_env()?;
    let mirror = match &config.archive_url {
        Some(url) => Some(archive::ArchiveMirror::new(
            url.clone(),
            config.archive_token_file.as_deref(),
        )?),
        None => None,
    };
    let node = DarmNode::open(config, now_ms())?;
    tracing::info!(node_id = %node.node_id(), chain = %node.ledger().config().name, "DARM-ANN node ready");
    let node = Arc::new(Mutex::new(node));
    let mirror_task = mirror.map(|mirror| tokio::spawn(mirror.run(Arc::clone(&node))));
    let result = PluginRuntime::run_with_stream(plugin(node), stream).await;
    if let Some(task) = mirror_task {
        task.abort();
    }
    result
}

pub async fn send_outbound(
    context: &mut PluginContext<'_>,
    outbound: Vec<Outbound>,
) -> anyhow::Result<()> {
    for item in outbound {
        context
            .send_json_channel(CHANNEL, item.target, item.message.kind(), &item.message)
            .await?;
    }
    Ok(())
}

pub fn plugin(node: SharedNode) -> SimplePlugin {
    let mut manifest = mesh_llm_plugin::plugin_manifest()
        .item(
            virtual_model(chat::VIRTUAL_MODEL, HANDLER)
                .supports_tools(true)
                .supports_streaming(true)
                .requires_candidates(true)
                .input_modalities(["text", "image", "audio"])
                .progress_lines(PROGRESS_LINES),
        )
        .item(mesh_channel(CHANNEL))
        .item(mesh_event_peer_up());
    for entry in ops::manifest_entries() {
        manifest = manifest.item(entry);
    }
    let manifest = manifest.build();

    let mut router = VirtualModelRouter::new();
    {
        let node = Arc::clone(&node);
        router.add_raw(
            operation_with_schema(
                HANDLER,
                "Run a DARM-ANN memory-augmented turn",
                serde_json::Map::new(),
            ),
            move |request, context| {
                let mut context = context.owned();
                let node = Arc::clone(&node);
                Box::pin(async move {
                    let invocation: VirtualModelInvocation = request.arguments()?;
                    let infer_context = context.clone();
                    let (response, outbound) =
                        chat::handle(invocation, node, |req| infer_context.infer(req)).await;
                    if let Err(error) = send_outbound(&mut context, outbound).await {
                        tracing::debug!(%error, "darm gossip send failed");
                    }
                    structured_tool_result(response)
                })
            },
        );
    }

    let init_node = Arc::clone(&node);
    let ticker_node = Arc::clone(&node);
    let channel_node = Arc::clone(&node);
    let event_node = Arc::clone(&node);
    let health_node = Arc::clone(&node);
    let ticker_started = Arc::new(AtomicBool::new(false));

    SimplePlugin::new(PluginMetadata::new(
        PLUGIN_ID,
        env!("CARGO_PKG_VERSION"),
        plugin_server_info(
            PLUGIN_ID,
            env!("CARGO_PKG_VERSION"),
            "DARM-ANN",
            "Distributed agentic memory, ledger and swarm routing for the mesh",
            None::<String>,
        ),
    ))
    .with_manifest(manifest)
    .with_virtual_model_router(router)
    .with_operation_router(ops::router(Arc::clone(&node)))
    .on_initialize(move |request, _context| {
        init_node
            .lock()
            .expect("darm node lock")
            .set_public_mesh(request.mesh_visibility == MeshVisibility::Public);
        Box::pin(async { Ok(()) })
    })
    .on_initialized(move |context| {
        if !ticker_started.swap(true, Ordering::SeqCst) {
            let mut context = context.owned();
            let node = Arc::clone(&ticker_node);
            tokio::spawn(async move {
                let mut interval = tokio::time::interval(TICK_INTERVAL);
                loop {
                    interval.tick().await;
                    let outbound = node.lock().expect("darm node lock").tick(now_ms());
                    if let Err(error) = send_outbound(&mut context, outbound).await {
                        // The host connection is gone: the plugin is shutting down.
                        tracing::debug!(%error, "darm ticker stopping");
                        break;
                    }
                }
            });
        }
        Box::pin(async { Ok(()) })
    })
    .on_channel_message(move |message, context| {
        let node = Arc::clone(&channel_node);
        Box::pin(async move {
            if message.channel != CHANNEL {
                return Ok(());
            }
            let wire = match Wire::decode(&message.body) {
                Ok(wire) => wire,
                Err(error) => {
                    tracing::debug!(%error, "darm dropped malformed gossip");
                    return Ok(());
                }
            };
            let outbound = node.lock().expect("darm node lock").handle(
                &message.source_peer_id,
                wire,
                now_ms(),
            );
            send_outbound(context, outbound).await
        })
    })
    .on_mesh_event(move |event, context| {
        let node = Arc::clone(&event_node);
        Box::pin(async move {
            if event.kind() != MeshEventKind::PeerUp {
                return Ok(());
            }
            let Some(peer) = event.peer.as_ref().map(|p| p.peer_id.clone()) else {
                return Ok(());
            };
            let outbound = node.lock().expect("darm node lock").on_peer_up(&peer);
            send_outbound(context, outbound).await
        })
    })
    .with_health(move |_context| {
        let node = Arc::clone(&health_node);
        Box::pin(async move {
            let node = node.lock().expect("darm node lock");
            Ok(format!(
                "node={} height={} memories={}",
                node.node_id().short(),
                node.ledger().height(),
                node.memory().len()
            ))
        })
    })
}
