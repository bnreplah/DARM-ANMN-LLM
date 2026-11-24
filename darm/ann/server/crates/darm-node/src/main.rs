//! `darm-node`: the DARM-ANN archive and anchor server.
//!
//! - `serve`: verifying archive of one chain's finalized ledger and its
//!   mesh-scoped memories, over HTTP (see `api.rs`).
//! - `keygen`: create a node identity offline and print its id (for
//!   assembling `DARM_VALIDATORS`).
//! - `vectors`: print the protocol test vectors other implementations must
//!   reproduce byte-for-byte.
//! - `replication-plan`: list `replicate` proposals that passed the
//!   validator vote and have not been executed, for an operator to act on.

mod api;
mod archive;
mod companion_vectors;
mod devices;
mod replication;
mod vectors;

use anyhow::{Context, bail};
use clap::{Parser, Subcommand};
use darm_core::{NodeId, NodeIdentity};
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

#[derive(Parser)]
#[command(
    name = "darm-node",
    version,
    about = "DARM-ANN archive and anchor node"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Serve the archive HTTP API.
    Serve {
        /// Address to listen on. Defaults to loopback; expose it through a
        /// TLS-terminating proxy or a private network, never directly.
        #[arg(long, env = "DARM_NODE_LISTEN", default_value = "127.0.0.1:8088")]
        listen: SocketAddr,
        #[arg(long, env = "DARM_NODE_DATA_DIR")]
        data_dir: Option<PathBuf>,
        /// Chain name, identical to the mesh nodes' DARM_CHAIN_NAME.
        #[arg(long, env = "DARM_CHAIN_NAME")]
        chain_name: String,
        /// Comma-separated validator node ids, identical to DARM_VALIDATORS.
        #[arg(long, env = "DARM_VALIDATORS", value_delimiter = ',')]
        validators: Vec<String>,
        /// File holding the bearer token for writes. Without it the node is
        /// read-only.
        #[arg(long, env = "DARM_NODE_WRITE_TOKEN_FILE")]
        write_token_file: Option<PathBuf>,
        /// File holding a bearer token required for reads (optional).
        #[arg(long, env = "DARM_NODE_READ_TOKEN_FILE")]
        read_token_file: Option<PathBuf>,
        /// Maximum concurrent requests.
        #[arg(long, default_value_t = 64)]
        max_concurrency: usize,
        /// Web origins allowed to read this node from a browser (GET only),
        /// e.g. https://you.github.io for the node monitor. Device companions
        /// from the same origins may also POST signed requests to
        /// /v1/devices. Comma-separated. Empty (the default) sends no CORS
        /// headers.
        #[arg(long, env = "DARM_NODE_CORS_ORIGINS", value_delimiter = ',')]
        cors_origins: Vec<String>,
        /// File holding a bearer token required to enroll a device
        /// companion (optional; without it enrollment is open, and still
        /// needs the device's own signature).
        #[arg(long, env = "DARM_NODE_DEVICE_ENROLL_TOKEN_FILE")]
        device_enroll_token_file: Option<PathBuf>,
        /// Feature activation heights, identical to the mesh's DARM_ACTIVATE,
        /// e.g. credits@1200,evidence@1200. Comma-separated.
        #[arg(long, env = "DARM_ACTIVATE", value_delimiter = ',')]
        activate: Vec<String>,
    },
    /// Create an identity key file (0600) and print its node id.
    Keygen {
        #[arg(long)]
        out: PathBuf,
    },
    /// Print protocol test vectors as JSON.
    Vectors,
    /// List passed `replicate` proposals not yet recorded as executed.
    ReplicationPlan {
        /// Archive base URL, e.g. http://127.0.0.1:8088
        #[arg(long)]
        archive: String,
        /// Local file recording executed proposal ids.
        #[arg(long)]
        state: PathBuf,
        /// Record a proposal id as executed after the operator acted on it.
        #[arg(long)]
        mark_done: Option<String>,
    },
}

fn read_token(path: &PathBuf) -> anyhow::Result<Arc<str>> {
    let token = std::fs::read_to_string(path)
        .with_context(|| format!("reading token file {}", path.display()))?;
    let token = token.trim();
    if token.len() < 32 {
        bail!("token in {} must be at least 32 characters", path.display());
    }
    Ok(Arc::from(token))
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();
    match Cli::parse().command {
        Command::Serve {
            listen,
            data_dir,
            chain_name,
            validators,
            write_token_file,
            read_token_file,
            max_concurrency,
            device_enroll_token_file,
            cors_origins,
            activate,
        } => {
            let activations =
                archive::parse_activations(&activate).context("parsing --activate")?;
            let cors = api::cors(&cors_origins).context("parsing --cors-origins")?;
            let validators = validators
                .iter()
                .map(|v| NodeId::from_hex(v.trim()))
                .collect::<Result<Vec<_>, _>>()
                .context("parsing --validators")?;
            let device_dir = data_dir.clone();
            let archive = archive::Archive::open(archive::ArchiveConfig {
                data_dir,
                chain_name,
                validators,
                activations,
            })?;
            tracing::info!(
                chain_id = %archive.ledger.chain_id(),
                height = archive.ledger.height(),
                "archive loaded and verified"
            );
            let chain_id = archive.ledger.chain_id();
            let devices = match &device_dir {
                Some(dir) => devices::Devices::open(chain_id, dir)?,
                None => devices::Devices::new(chain_id),
            };
            tracing::info!(
                devices = devices.len(),
                "device registry loaded and verified"
            );
            let state = api::AppState {
                archive: Arc::new(Mutex::new(archive)),
                devices: Arc::new(Mutex::new(devices)),
                enroll_token: device_enroll_token_file
                    .as_ref()
                    .map(read_token)
                    .transpose()?,
                write_token: write_token_file.as_ref().map(read_token).transpose()?,
                read_token: read_token_file.as_ref().map(read_token).transpose()?,
            };
            if state.write_token.is_none() {
                tracing::warn!("no write token configured: node is read-only");
            }
            if cors.is_some() {
                tracing::info!(origins = ?cors_origins, "browser access enabled");
            }
            let app = api::router_with_cors(state, cors)
                .layer(tower::limit::ConcurrencyLimitLayer::new(
                    max_concurrency.max(1),
                ))
                .layer(tower_http::trace::TraceLayer::new_for_http());
            let listener = tokio::net::TcpListener::bind(listen).await?;
            tracing::info!(%listen, "darm-node listening");
            axum::serve(listener, app)
                .with_graceful_shutdown(async {
                    let _ = tokio::signal::ctrl_c().await;
                })
                .await?;
        }
        Command::Keygen { out } => {
            if out.exists() {
                bail!(
                    "{} already exists; refusing to overwrite a key",
                    out.display()
                );
            }
            let identity = NodeIdentity::load_or_create(&out)?;
            println!("{}", identity.node_id());
        }
        Command::Vectors => {
            println!("{}", serde_json::to_string_pretty(&vectors::generate()?)?);
        }
        Command::ReplicationPlan {
            archive,
            state,
            mark_done,
        } => replication::run(&archive, &state, mark_done.as_deref()).await?,
    }
    Ok(())
}
