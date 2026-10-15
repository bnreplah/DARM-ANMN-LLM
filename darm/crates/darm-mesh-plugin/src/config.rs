//! Plugin configuration, read from `DARM_*` environment variables.
//!
//! Defaults are conservative: memories stay on the node (`share = false`),
//! peer memories are never injected into prompts unless their author is
//! explicitly trusted, nothing is gossiped on a public mesh without an
//! explicit opt-in, and replication is never proposed automatically.

use darm_core::{NodeId, Result};
use std::path::PathBuf;

#[derive(Debug, Clone)]
pub struct DarmConfig {
    pub data_dir: Option<PathBuf>,
    /// Ledger chain name. Every validator of one chain must use the same.
    pub chain_name: Option<String>,
    /// Ledger validators. Empty means "this node alone".
    pub validators: Vec<NodeId>,
    /// Authors whose mesh memories may be injected into prompts.
    pub trusted_authors: Vec<NodeId>,
    /// Store chat exchanges as (local) episodic memories.
    pub remember: bool,
    /// Make new memories mesh-scoped and gossip them.
    pub share: bool,
    /// Allow memory/ledger gossip on a public mesh.
    pub allow_public_gossip: bool,
    /// Submit `replicate` proposals when the swarm policy recommends it.
    pub propose_replication: bool,
    /// Number of memories injected per turn.
    pub recall_top_k: usize,
    /// Character budget for injected memory context.
    pub recall_budget_chars: usize,
    /// Concurrent requests this node considers full capacity.
    pub capacity: u32,
    /// Latency the lifecycle considers healthy.
    pub target_latency_ms: f64,
    /// DARM-ANN archive node to mirror finalized blocks and shared memories to.
    pub archive_url: Option<String>,
    /// File holding the archive's write bearer token.
    pub archive_token_file: Option<PathBuf>,
}

impl Default for DarmConfig {
    fn default() -> Self {
        Self {
            data_dir: None,
            chain_name: None,
            validators: Vec::new(),
            trusted_authors: Vec::new(),
            remember: true,
            share: false,
            allow_public_gossip: false,
            propose_replication: false,
            recall_top_k: 5,
            recall_budget_chars: 4_000,
            capacity: 8,
            target_latency_ms: 8_000.0,
            archive_url: None,
            archive_token_file: None,
        }
    }
}

fn flag(name: &str, default: bool) -> bool {
    match std::env::var(name) {
        Ok(value) => matches!(
            value.trim().to_ascii_lowercase().as_str(),
            "1" | "true" | "yes" | "on"
        ),
        Err(_) => default,
    }
}

fn number<T: std::str::FromStr>(name: &str, default: T) -> T {
    std::env::var(name)
        .ok()
        .and_then(|v| v.trim().parse().ok())
        .unwrap_or(default)
}

fn node_list(name: &str) -> Result<Vec<NodeId>> {
    let Ok(value) = std::env::var(name) else {
        return Ok(Vec::new());
    };
    value
        .split([',', ' ', '\n'])
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(NodeId::from_hex)
        .collect()
}

impl DarmConfig {
    pub fn from_env() -> Result<Self> {
        let defaults = Self::default();
        let data_dir = std::env::var_os("DARM_DATA_DIR")
            .map(PathBuf::from)
            .or_else(|| {
                std::env::var_os("HOME")
                    .or_else(|| std::env::var_os("USERPROFILE"))
                    .map(|home| PathBuf::from(home).join(".mesh-llm").join("darm"))
            });
        Ok(Self {
            data_dir,
            chain_name: std::env::var("DARM_CHAIN_NAME")
                .ok()
                .filter(|s| !s.trim().is_empty()),
            validators: node_list("DARM_VALIDATORS")?,
            trusted_authors: node_list("DARM_TRUSTED_AUTHORS")?,
            remember: flag("DARM_REMEMBER", defaults.remember),
            share: flag("DARM_SHARE", defaults.share),
            allow_public_gossip: flag("DARM_ALLOW_PUBLIC_GOSSIP", defaults.allow_public_gossip),
            propose_replication: flag("DARM_PROPOSE_REPLICATION", defaults.propose_replication),
            recall_top_k: number("DARM_RECALL_TOP_K", defaults.recall_top_k).clamp(0, 20),
            recall_budget_chars: number("DARM_RECALL_BUDGET_CHARS", defaults.recall_budget_chars)
                .clamp(0, 32_000),
            capacity: number("DARM_CAPACITY", defaults.capacity).max(1),
            target_latency_ms: number("DARM_TARGET_LATENCY_MS", defaults.target_latency_ms)
                .max(1.0),
            archive_url: match std::env::var("DARM_ARCHIVE_URL") {
                Ok(url) if !url.trim().is_empty() => Some(validate_archive_url(url.trim())?),
                _ => None,
            },
            archive_token_file: std::env::var_os("DARM_ARCHIVE_TOKEN_FILE").map(PathBuf::from),
        })
    }
}

/// The archive receives the node's shared memories and its write token, so
/// plaintext HTTP is only allowed to the local machine.
pub fn validate_archive_url(url: &str) -> Result<String> {
    let url = url.trim_end_matches('/');
    let lower = url.to_ascii_lowercase();
    let loopback = ["http://127.0.0.1", "http://localhost", "http://[::1]"]
        .iter()
        .any(|prefix| {
            lower.strip_prefix(prefix).is_some_and(|rest| {
                rest.is_empty() || rest.starts_with(':') || rest.starts_with('/')
            })
        });
    if lower.starts_with("https://") || loopback {
        Ok(url.to_string())
    } else {
        Err(darm_core::DarmError::Rejected(
            "DARM_ARCHIVE_URL must be https:// (plain http only to loopback)".into(),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn archive_url_requires_tls_off_loopback() {
        assert!(validate_archive_url("https://archive.example/").is_ok());
        assert!(validate_archive_url("http://127.0.0.1:8088").is_ok());
        assert!(validate_archive_url("http://localhost").is_ok());
        assert!(validate_archive_url("http://archive.example").is_err());
        assert!(validate_archive_url("http://127.0.0.1.evil.example").is_err());
        assert!(validate_archive_url("http://localhost.evil.example:80").is_err());
        assert!(validate_archive_url("ftp://x").is_err());
    }
}
