//! Optional mirror to a DARM-ANN archive node (`darm-node`, in the DARM-ANN
//! infrastructure repository). Every interval it pushes finalized blocks the
//! archive does not have yet and mesh-scoped memories it has not accepted.
//! The archive re-verifies everything; the token only authorizes writing.

use crate::SharedNode;
use anyhow::{Context, bail};
use std::path::Path;
use std::time::Duration;

const INTERVAL: Duration = Duration::from_secs(30);
const BATCH: usize = 64;

pub struct ArchiveMirror {
    base: String,
    token: String,
    client: reqwest::Client,
}

impl ArchiveMirror {
    pub fn new(base: String, token_file: Option<&Path>) -> anyhow::Result<Self> {
        let Some(token_file) = token_file else {
            bail!("DARM_ARCHIVE_URL is set but DARM_ARCHIVE_TOKEN_FILE is not");
        };
        let token = std::fs::read_to_string(token_file)
            .with_context(|| format!("reading {}", token_file.display()))?
            .trim()
            .to_string();
        if token.len() < 32 {
            bail!("archive token must be at least 32 characters");
        }
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(30))
            // Never follow a redirect: it could carry the token elsewhere.
            .redirect(reqwest::redirect::Policy::none())
            .build()?;
        Ok(Self {
            base,
            token,
            client,
        })
    }

    async fn archive_height(&self) -> anyhow::Result<u64> {
        let info: serde_json::Value = self
            .client
            .get(format!("{}/v1/info", self.base))
            .bearer_auth(&self.token)
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;
        info["height"]
            .as_u64()
            .context("archive info without height")
    }

    /// One mirror pass. Returns `(blocks pushed, memories accepted)`.
    pub async fn sync_once(&self, node: &SharedNode) -> anyhow::Result<(usize, usize)> {
        let height = self.archive_height().await?;
        let blocks = node
            .lock()
            .expect("darm node lock")
            .blocks_after(height, BATCH);
        if !blocks.is_empty() {
            self.client
                .post(format!("{}/v1/ledger/blocks", self.base))
                .bearer_auth(&self.token)
                .json(&blocks)
                .send()
                .await?
                .error_for_status()?;
        }
        let memories = node
            .lock()
            .expect("darm node lock")
            .unarchived_memories(BATCH);
        let mut accepted = 0;
        if !memories.is_empty() {
            let results: Vec<serde_json::Value> = self
                .client
                .post(format!("{}/v1/memories", self.base))
                .bearer_auth(&self.token)
                .json(&memories)
                .send()
                .await?
                .error_for_status()?
                .json()
                .await?;
            // Stored or already known both count as archived; rejected
            // records are retried on a later pass.
            let ids: Vec<darm_core::Hash32> = results
                .iter()
                .filter(|r| r.get("error").is_none())
                .filter_map(|r| r["id"].as_str())
                .filter_map(|id| darm_core::Hash32::from_hex(id).ok())
                .collect();
            accepted = ids.len();
            node.lock().expect("darm node lock").mark_archived(ids);
        }
        Ok((blocks.len(), accepted))
    }

    pub async fn run(self, node: SharedNode) {
        let mut interval = tokio::time::interval(INTERVAL);
        loop {
            interval.tick().await;
            match self.sync_once(&node).await {
                Ok((0, 0)) => {}
                Ok((blocks, memories)) => {
                    tracing::debug!(blocks, memories, "darm mirrored to archive");
                }
                Err(error) => tracing::warn!(%error, "darm archive mirror failed"),
            }
        }
    }
}
