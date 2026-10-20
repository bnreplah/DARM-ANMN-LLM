//! Plugin configuration, read from `DARM_*` environment variables.
//!
//! Defaults are conservative: memories stay on the node (`share = false`),
//! peer memories are never injected into prompts unless their author is
//! explicitly trusted, nothing is gossiped on a public mesh without an
//! explicit opt-in, and replication is never proposed automatically.

use darm_compute::{ContributionPolicy, Device, FormFactor, WorkloadKind};
use darm_core::{DarmError, NodeId, Result};
use std::path::PathBuf;

/// Whether gossip runs inside authenticated Noise sessions.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionMode {
    /// Plaintext signed gossip only (pre-session behaviour).
    Off,
    /// Establish sessions with every peer and seal traffic to peers that
    /// have one; plaintext signed gossip still flows to peers that do not.
    Preferred,
    /// Only handshakes travel in the clear; everything else is sealed, and
    /// plaintext gossip from peers is dropped. Forced on public meshes.
    Required,
}

impl SessionMode {
    fn parse(value: &str) -> Result<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "off" | "0" | "false" => Ok(Self::Off),
            "preferred" | "on" | "1" | "true" => Ok(Self::Preferred),
            "required" | "strict" => Ok(Self::Required),
            other => Err(DarmError::Rejected(format!(
                "DARM_SESSIONS must be off|preferred|required, got {other:?}"
            ))),
        }
    }
}

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
    /// Join the compute pool (advertise this node's CPUs/GPUs).
    pub compute_pool: bool,
    pub form_factor: FormFactor,
    /// Operator-declared accelerators (`DARM_GPUS`).
    pub gpus: Vec<Device>,
    /// Workload kinds this node's operator agrees to serve.
    pub workloads: Vec<WorkloadKind>,
    /// What the device owner contributes, and under which conditions.
    pub policy: ContributionPolicy,
    /// Start in a metered-network state (phones on cellular).
    pub metered: bool,
    pub sessions: SessionMode,
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
            compute_pool: true,
            form_factor: FormFactor::Server,
            gpus: Vec::new(),
            workloads: vec![WorkloadKind::Inference, WorkloadKind::Embedding],
            policy: ContributionPolicy::for_form_factor(FormFactor::Server),
            metered: false,
            sessions: SessionMode::Preferred,
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

fn parse_list<T>(name: &str, parse: impl Fn(&str) -> Option<T>) -> Result<Option<Vec<T>>> {
    let Ok(value) = std::env::var(name) else {
        return Ok(None);
    };
    value
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|item| {
            parse(item)
                .ok_or_else(|| DarmError::Rejected(format!("{name}: unknown value {item:?}")))
        })
        .collect::<Result<Vec<T>>>()
        .map(Some)
}

/// Contribution policy: form-factor defaults, then explicit overrides.
fn policy_from_env(form_factor: FormFactor) -> Result<ContributionPolicy> {
    let mut policy = ContributionPolicy::for_form_factor(form_factor);
    policy.contribute = flag("DARM_CONTRIBUTE", policy.contribute);
    if let Some(devices) = parse_list("DARM_POOL_DEVICES", |d| {
        match d.to_ascii_lowercase().as_str() {
            "cpu" => Some(true),
            "gpu" => Some(false),
            _ => None,
        }
    })? {
        policy.allow_cpu = devices.contains(&true);
        policy.allow_gpu = devices.contains(&false);
    }
    policy.max_share_pct = number("DARM_POOL_SHARE_PCT", policy.max_share_pct).clamp(1, 100);
    policy.require_charging = flag("DARM_POOL_REQUIRE_CHARGING", policy.require_charging);
    policy.min_battery_pct = number("DARM_POOL_MIN_BATTERY_PCT", policy.min_battery_pct).min(100);
    policy.allow_metered = flag("DARM_POOL_ALLOW_METERED", policy.allow_metered);
    Ok(policy)
}

impl DarmConfig {
    pub fn from_env() -> Result<Self> {
        let defaults = Self::default();
        let form_factor = match std::env::var("DARM_FORM_FACTOR") {
            Ok(value) => FormFactor::parse(&value).ok_or_else(|| {
                DarmError::Rejected(format!("DARM_FORM_FACTOR: unknown form factor {value:?}"))
            })?,
            Err(_) => defaults.form_factor,
        };
        let gpus = match std::env::var("DARM_GPUS") {
            Ok(value) => value
                .split(',')
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .take(darm_compute::device::MAX_DEVICES - 1)
                .map(Device::parse_gpu)
                .collect::<Result<Vec<_>>>()?,
            Err(_) => Vec::new(),
        };
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
            compute_pool: flag("DARM_COMPUTE_POOL", defaults.compute_pool),
            form_factor,
            gpus,
            workloads: parse_list("DARM_WORKLOADS", WorkloadKind::parse)?
                .filter(|w| !w.is_empty())
                .unwrap_or(defaults.workloads),
            policy: policy_from_env(form_factor)?,
            metered: flag("DARM_METERED", defaults.metered),
            sessions: match std::env::var("DARM_SESSIONS") {
                Ok(value) => SessionMode::parse(&value)?,
                Err(_) => defaults.sessions,
            },
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
    fn session_modes_parse_strictly() {
        assert_eq!(
            SessionMode::parse("Required").unwrap(),
            SessionMode::Required
        );
        assert_eq!(SessionMode::parse("off").unwrap(), SessionMode::Off);
        assert!(SessionMode::parse("maybe").is_err());
    }

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
