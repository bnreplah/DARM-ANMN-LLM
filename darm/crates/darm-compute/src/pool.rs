//! The compute pool: live adverts plus a heterogeneous CPU/GPU scheduler.
//!
//! Scheduling weight of one (node, device) slot:
//!
//! ```text
//! weight = gflops_prior × share × efficiency_correction × reliability × trust
//!          × availability × preference × mobile_factor
//! ```
//!
//! - `efficiency_correction` is the node's measured rate per advertised
//!   GFLOP divided by the pool mean for that device kind; unmeasured nodes
//!   start on probation at 0.75. A node that overstates its hardware is
//!   measured down to what it actually delivers.
//! - `reliability` is the Laplace-smoothed success rate of its receipts.
//! - `trust` comes from the caller (ledger reputation / trust set).
//!
//! Shards are dealt by smooth weighted round robin, so work is split across
//! CPU and GPU slots in proportion to their weights and interleaved rather
//! than front-loaded. Replicas of one shard always land on distinct nodes.

use crate::admission::Threshold;
use crate::advert::SignedAdvert;
use crate::device::{Backend, DeviceKind, FormFactor, Thermal, WorkloadKind};
use darm_core::canonical::Canonical;
use darm_core::ledger::MAX_COMPUTE_SHARDS;
use darm_core::{DarmError, Hash32, NodeId, Result, merkle};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap};

pub const DEFAULT_MAX_MEMBERS: usize = 4_096;
/// Most shards one plan may hold (the ledger allows more; planning is
/// bounded so a caller cannot make a node do unbounded scheduling work).
pub const MAX_PLAN_SHARDS: usize = 4_096;
pub const MAX_REDUNDANCY: u8 = 7;
/// Only the best slots are considered when dealing shards.
const MAX_SLOTS: usize = 256;
const PROBATION: f64 = 0.75;
const MIN_SAMPLES: u32 = 3;
/// Volume 1 §11 ramps a new node's vote weight over `t_min_age` (60 s in
/// the paper). Volume 1.1 §3 notes 60 s is sized for a large, mature
/// network, not a bootstrapping one, so the pool defaults to ten minutes.
pub const DEFAULT_MIN_AGE_MS: u64 = 10 * 60 * 1_000;
/// Weight multiplier for a replica candidate that shares a backend with a
/// replica already placed on the same shard (correlated-failure reduction,
/// Paper 2 / Volume 1.1 §3: diversity lowers ρ).
const SAME_BACKEND_PENALTY: f64 = 0.25;
/// Locality Doctrine (v7.2 §4.5): `pipeline` work needs RTT ≤ 30 ms.
pub const PIPELINE_MAX_RTT_MS: f64 = 30.0;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DevicePreference {
    #[default]
    Any,
    CpuOnly,
    GpuOnly,
    PreferGpu,
    PreferCpu,
}

impl DevicePreference {
    fn code(self) -> u8 {
        match self {
            Self::Any => 0,
            Self::CpuOnly => 1,
            Self::GpuOnly => 2,
            Self::PreferGpu => 3,
            Self::PreferCpu => 4,
        }
    }

    fn factor(self, kind: DeviceKind) -> f64 {
        let accelerator = kind != DeviceKind::Cpu;
        match (self, accelerator) {
            (Self::Any, _) => 1.0,
            (Self::CpuOnly, false) | (Self::GpuOnly, true) => 1.0,
            (Self::CpuOnly, true) | (Self::GpuOnly, false) => 0.0,
            (Self::PreferGpu, true) | (Self::PreferCpu, false) => 2.0,
            (Self::PreferGpu, false) | (Self::PreferCpu, true) => 0.5,
        }
    }
}

/// Locality Doctrine (v7.2 §4.5): how tightly a job's workers must be
/// coupled. Inference and memory work is WAN-tolerant; training is not.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SyncClass {
    /// Inference, memory queries, post-training work: anywhere.
    #[default]
    Async,
    /// Block-local training objectives: WAN-tolerant, independently verified.
    Block,
    /// Pipeline-parallel: regional, RTT ≤ 30 ms.
    Pipeline,
    /// Tensor-parallel / full-model training: this node only.
    Tight,
}

impl SyncClass {
    fn code(self) -> u8 {
        match self {
            Self::Async => 0,
            Self::Block => 1,
            Self::Pipeline => 2,
            Self::Tight => 3,
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        Some(match value.trim().to_ascii_lowercase().as_str() {
            "async" => Self::Async,
            "block" => Self::Block,
            "pipeline" => Self::Pipeline,
            "tight" => Self::Tight,
            _ => return None,
        })
    }
}

/// What the planner needs to know beyond the pool itself.
pub struct PlanContext<'a> {
    /// Trust in `[0, 1]` per worker; 0 excludes it.
    pub trust: &'a dyn Fn(&NodeId) -> f64,
    /// Measured round-trip time to a worker, if known.
    pub rtt_ms: &'a dyn Fn(&NodeId) -> Option<f64>,
}

/// What a coordinator wants computed. Each shard is identified by the hash
/// of its input, so workers sign over exactly what they were given.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct JobSpec {
    pub workload: WorkloadKind,
    #[serde(default)]
    pub device: DevicePreference,
    pub inputs: Vec<Hash32>,
    /// Independent workers per shard. 3+ lets a majority outvote a liar;
    /// 2 detects disagreement; 1 is unverified.
    pub redundancy: u8,
    #[serde(default)]
    pub min_memory_mb: u32,
    #[serde(default)]
    pub allow_mobile: bool,
    /// Keep all replicas of a shard on the same device kind. Needed when
    /// outputs are floating point and CPU and GPU kernels round differently.
    #[serde(default)]
    pub same_kind_replicas: bool,
    /// Admission threshold τ_c over each shard's replicas. Under correlated
    /// validators this is a stronger safety lever than adding replicas
    /// (Paper 2 H1), so it is a first-class job parameter.
    #[serde(default)]
    pub threshold: Threshold,
    /// Probability (per mille) that a shard gets one extra, silently
    /// placed replica (v7.2 §4.3 tier 1, bounds P66/P67).
    #[serde(default)]
    pub spot_check_milli: u16,
    /// Prefer replicas on different backends (lower correlation).
    #[serde(default = "default_true")]
    pub diverse_replicas: bool,
    #[serde(default)]
    pub sync_class: SyncClass,
    #[serde(default)]
    pub nonce: u64,
}

fn default_true() -> bool {
    true
}

impl JobSpec {
    pub fn validate(&self) -> Result<()> {
        if self.inputs.is_empty() || self.inputs.len() > MAX_PLAN_SHARDS {
            return Err(DarmError::malformed("job", "1..=4096 shards"));
        }
        debug_assert!(MAX_PLAN_SHARDS as u32 <= MAX_COMPUTE_SHARDS);
        if !(1..=MAX_REDUNDANCY).contains(&self.redundancy) {
            return Err(DarmError::malformed("job", "redundancy must be 1..=7"));
        }
        if self.spot_check_milli > 1_000 {
            return Err(DarmError::malformed(
                "job",
                "spot_check_milli must be 0..=1000",
            ));
        }
        self.threshold.validate()
    }

    /// Whether `shard` carries a spot-check replica: a keyed hash of the
    /// job's inputs and nonce, so workers cannot predict it from their own
    /// shard alone.
    pub fn spot_checked(&self, shard: u32) -> bool {
        if self.spot_check_milli == 0 {
            return false;
        }
        let mut encoder = Canonical::new("compute-spot-check");
        encoder
            .fixed32(self.input_root().as_bytes())
            .u64(self.nonce)
            .u32(shard);
        let digest = Hash32::digest("compute-spot-check", encoder.as_bytes());
        let roll = u16::from_be_bytes([digest.0[0], digest.0[1]]) % 1_000;
        roll < self.spot_check_milli
    }

    /// Replicas planned for `shard`.
    pub fn replicas(&self, shard: u32) -> u32 {
        u32::from(self.redundancy) + u32::from(self.spot_checked(shard))
    }

    pub fn input_root(&self) -> Hash32 {
        merkle::root(&self.inputs)
    }

    /// Job id: commits to the coordinator, network and every parameter, so
    /// it cannot be claimed by anyone else on the ledger.
    pub fn job_id(&self, coordinator: &NodeId, network: &Hash32) -> Hash32 {
        let mut encoder = Canonical::new("compute-job");
        encoder
            .fixed32(coordinator.as_bytes())
            .fixed32(network.as_bytes())
            .u8(self.workload.code())
            .u8(self.device.code())
            .fixed32(self.input_root().as_bytes())
            .u32(self.inputs.len() as u32)
            .u8(self.redundancy)
            .u32(self.min_memory_mb)
            .bool(self.allow_mobile)
            .bool(self.same_kind_replicas)
            .u16(self.threshold.num)
            .u16(self.threshold.den)
            .u16(self.spot_check_milli)
            .bool(self.diverse_replicas)
            .u8(self.sync_class.code())
            .u64(self.nonce);
        Hash32::digest("compute-job", encoder.as_bytes())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Assignment {
    pub shard: u32,
    pub replica: u8,
    pub worker: NodeId,
    pub device: DeviceKind,
    pub backend: Backend,
    pub mesh_peer: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct JobPlan {
    pub job: Hash32,
    pub coordinator: NodeId,
    pub spec: JobSpec,
    pub assignments: Vec<Assignment>,
    pub cpu_shards: u32,
    pub gpu_shards: u32,
    pub npu_shards: u32,
}

impl JobPlan {
    pub fn assigned(&self, shard: u32, worker: &NodeId) -> bool {
        self.assignments
            .iter()
            .any(|a| a.shard == shard && a.worker == *worker)
    }

    /// Replicas actually placed on `shard` (spot checks included).
    pub fn replicas(&self, shard: u32) -> u32 {
        self.assignments.iter().filter(|a| a.shard == shard).count() as u32
    }

    /// Shards that carry a spot-check replica.
    pub fn spot_checked_shards(&self) -> u32 {
        (0..self.spec.inputs.len() as u32)
            .filter(|s| self.replicas(*s) > u32::from(self.spec.redundancy))
            .count() as u32
    }

    /// Distinct backends per shard, averaged: a coarse, observable proxy for
    /// how diverse (and so how weakly correlated) the replicas are.
    pub fn mean_backend_diversity(&self) -> f64 {
        let shards = self.spec.inputs.len().max(1);
        let total: usize = (0..self.spec.inputs.len() as u32)
            .map(|shard| {
                let mut seen: Vec<Backend> = Vec::new();
                for a in self.assignments.iter().filter(|a| a.shard == shard) {
                    if !seen.contains(&a.backend) {
                        seen.push(a.backend);
                    }
                }
                seen.len()
            })
            .sum();
        total as f64 / shards as f64
    }
}

#[derive(Debug, Clone, Default)]
struct Efficiency {
    /// EWMA of measured units/second per advertised GFLOP.
    per_gflop: f64,
    samples: u32,
}

#[derive(Debug, Clone)]
struct Member {
    advert: SignedAdvert,
    first_seen_ms: u64,
    peer_verified: bool,
    efficiency: HashMap<DeviceKind, Efficiency>,
    successes: u32,
    failures: u32,
}

impl Member {
    fn reliability(&self) -> f64 {
        (f64::from(self.successes) + 1.0) / (f64::from(self.successes + self.failures) + 2.0)
    }

    fn availability(&self) -> f64 {
        let c = &self.advert.advert.conditions;
        let thermal = match c.thermal {
            Thermal::Nominal => 1.0,
            Thermal::Fair => 0.8,
            Thermal::Serious => 0.4,
            Thermal::Critical => 0.0,
        };
        (1.0 - f64::from(c.load_milli) / 1_000.0).max(0.05) * thermal
    }

    fn mobile_factor(&self) -> f64 {
        let advert = &self.advert.advert;
        let mut factor = 1.0;
        if advert.form_factor.is_mobile() {
            factor *= 0.5;
        }
        if advert.conditions.on_battery && !advert.conditions.charging {
            factor *= 0.5;
        }
        factor
    }
}

/// Aggregate view for status pages.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct PoolSummary {
    pub nodes: usize,
    pub mobile_nodes: usize,
    pub cpu_devices: usize,
    pub cpu_cores: u64,
    pub gpu_devices: usize,
    pub npu_devices: usize,
    pub accelerator_memory_mb: u64,
    pub cpu_gflops: u64,
    pub accelerator_gflops: u64,
}

#[derive(Debug, Clone)]
pub struct ComputePool {
    network: Hash32,
    members: BTreeMap<NodeId, Member>,
    by_peer: HashMap<String, NodeId>,
    max_members: usize,
    min_age_ms: u64,
    local: Option<NodeId>,
}

struct Slot {
    node: NodeId,
    kind: DeviceKind,
    backend: Backend,
    weight: f64,
    current: f64,
}

impl ComputePool {
    pub fn new(network: Hash32) -> Self {
        Self::with_capacity(network, DEFAULT_MAX_MEMBERS)
    }

    pub fn with_capacity(network: Hash32, max_members: usize) -> Self {
        Self {
            network,
            members: BTreeMap::new(),
            by_peer: HashMap::new(),
            max_members: max_members.max(1),
            min_age_ms: DEFAULT_MIN_AGE_MS,
            local: None,
        }
    }

    /// Weight ramp for newly seen nodes (0 disables it).
    pub fn with_min_age(mut self, min_age_ms: u64) -> Self {
        self.min_age_ms = min_age_ms;
        self
    }

    /// The local node is exempt from the new-node ramp.
    pub fn with_local(mut self, node: NodeId) -> Self {
        self.local = Some(node);
        self
    }

    /// `VoteWeight(t) = min(1, (t − t0)/t_min_age)` (Volume 1 §11).
    fn age_weight(&self, node: &NodeId, member: &Member, now_ms: u64) -> f64 {
        if self.min_age_ms == 0 || self.local == Some(*node) {
            return 1.0;
        }
        (now_ms.saturating_sub(member.first_seen_ms) as f64 / self.min_age_ms as f64).min(1.0)
    }

    pub fn len(&self) -> usize {
        self.members.len()
    }

    pub fn is_empty(&self) -> bool {
        self.members.is_empty()
    }

    pub fn advert(&self, node: &NodeId) -> Option<&SignedAdvert> {
        self.members.get(node).map(|m| &m.advert)
    }

    /// Admit a verified advert.
    ///
    /// `source_peer` is the mesh peer the advert arrived from and
    /// `peer_verified` says whether that source is proven (an authenticated
    /// session with the advertising node, or the local node itself). The
    /// advert's `mesh_peer` claim is only mapped when it equals the source,
    /// and an unverified claim never displaces a verified one.
    pub fn upsert(
        &mut self,
        signed: SignedAdvert,
        source_peer: Option<&str>,
        peer_verified: bool,
        now_ms: u64,
    ) -> Result<bool> {
        signed.verify(&self.network, now_ms)?;
        let node = signed.advert.node;
        if let Some(existing) = self.members.get(&node)
            && existing.advert.advert.seq >= signed.advert.seq
        {
            return Ok(false);
        }
        if !self.members.contains_key(&node) && self.members.len() >= self.max_members {
            self.expire(now_ms);
            if self.members.len() >= self.max_members {
                return Err(DarmError::LimitExceeded("compute pool full".into()));
            }
        }
        let claim = signed.advert.mesh_peer.clone();
        // A host-signed binding proves the endpoint whatever the frame said.
        let binding_valid = signed.advert.mesh_binding_valid();
        let peer_verified = peer_verified || binding_valid;
        let bound = match (&claim, source_peer) {
            (Some(_), _) if binding_valid => true,
            (Some(claim), Some(source)) if claim == source => {
                let displaces_verified = self.by_peer.get(claim).is_some_and(|owner| {
                    *owner != node && self.members.get(owner).is_some_and(|m| m.peer_verified)
                });
                !displaces_verified || peer_verified
            }
            _ => false,
        };
        let member = self.members.entry(node).or_insert_with(|| Member {
            advert: signed.clone(),
            first_seen_ms: now_ms,
            peer_verified: false,
            efficiency: HashMap::new(),
            successes: 0,
            failures: 0,
        });
        member.advert = signed;
        if bound {
            member.peer_verified = peer_verified;
            let claim = claim.expect("bound implies a claim");
            if let Some(previous) = self.by_peer.insert(claim.clone(), node)
                && previous != node
                && let Some(other) = self.members.get_mut(&previous)
            {
                other.peer_verified = false;
            }
        }
        Ok(true)
    }

    pub fn expire(&mut self, now_ms: u64) {
        self.members
            .retain(|_, m| m.advert.advert.expires_ms() > now_ms);
        let members = &self.members;
        self.by_peer.retain(|_, node| members.contains_key(node));
    }

    pub fn node_for_peer(&self, peer: &str) -> Option<NodeId> {
        self.by_peer.get(peer).copied()
    }

    /// Feed one measured shard back into the scheduler.
    pub fn record(
        &mut self,
        worker: &NodeId,
        kind: DeviceKind,
        units: u32,
        elapsed_ms: u32,
        ok: bool,
    ) {
        let Some(member) = self.members.get_mut(worker) else {
            return;
        };
        if !ok {
            member.failures = member.failures.saturating_add(1);
            return;
        }
        member.successes = member.successes.saturating_add(1);
        let gflops: u64 = member
            .advert
            .advert
            .devices
            .iter()
            .filter(|d| d.device.kind == kind)
            .map(|d| u64::from(d.device.gflops))
            .sum();
        if gflops == 0 || units == 0 {
            return;
        }
        let rate = f64::from(units) / (f64::from(elapsed_ms.max(1)) / 1_000.0);
        let sample = rate / gflops as f64;
        let entry = member.efficiency.entry(kind).or_default();
        entry.per_gflop = if entry.samples == 0 {
            sample
        } else {
            0.8 * entry.per_gflop + 0.2 * sample
        };
        entry.samples = entry.samples.saturating_add(1);
    }

    fn mean_efficiency(&self, kind: DeviceKind) -> Option<f64> {
        let measured: Vec<f64> = self
            .members
            .values()
            .filter_map(|m| m.efficiency.get(&kind))
            .filter(|e| e.samples >= MIN_SAMPLES)
            .map(|e| e.per_gflop)
            .collect();
        (!measured.is_empty()).then(|| measured.iter().sum::<f64>() / measured.len() as f64)
    }

    fn correction(&self, member: &Member, kind: DeviceKind, mean: Option<f64>) -> f64 {
        match (member.efficiency.get(&kind), mean) {
            (Some(e), Some(mean)) if e.samples >= MIN_SAMPLES && mean > 0.0 => {
                (e.per_gflop / mean).clamp(0.01, 4.0)
            }
            _ => PROBATION,
        }
    }

    fn slots(
        &self,
        spec: &JobSpec,
        coordinator: &NodeId,
        context: &PlanContext<'_>,
        now_ms: u64,
    ) -> Vec<Slot> {
        let means: HashMap<DeviceKind, Option<f64>> =
            [DeviceKind::Cpu, DeviceKind::Gpu, DeviceKind::Npu]
                .into_iter()
                .map(|k| (k, self.mean_efficiency(k)))
                .collect();
        let mut slots = Vec::new();
        for (node, member) in &self.members {
            let advert = &member.advert.advert;
            if advert.expires_ms() <= now_ms
                || !advert.workloads.contains(&spec.workload)
                || (advert.form_factor.is_mobile() && !spec.allow_mobile)
            {
                continue;
            }
            let local_enough = match spec.sync_class {
                SyncClass::Async | SyncClass::Block => true,
                SyncClass::Tight => node == coordinator,
                SyncClass::Pipeline => {
                    node == coordinator
                        || (context.rtt_ms)(node).is_some_and(|rtt| rtt <= PIPELINE_MAX_RTT_MS)
                }
            };
            let trust =
                (context.trust)(node).clamp(0.0, 1.0) * self.age_weight(node, member, now_ms);
            if !local_enough || trust <= 0.0 {
                continue;
            }
            let base =
                member.reliability() * trust * member.availability() * member.mobile_factor();
            for offered in &advert.devices {
                let d = &offered.device;
                let preference = spec.device.factor(d.kind);
                if preference <= 0.0 || d.memory_mb < spec.min_memory_mb {
                    continue;
                }
                let weight = f64::from(d.gflops) * f64::from(offered.share_pct) / 100.0
                    * self.correction(member, d.kind, means[&d.kind])
                    * base
                    * preference;
                if weight > 0.0 && weight.is_finite() {
                    slots.push(Slot {
                        node: *node,
                        kind: d.kind,
                        backend: d.backend,
                        weight,
                        current: 0.0,
                    });
                }
            }
        }
        slots.sort_by(|a, b| {
            b.weight
                .total_cmp(&a.weight)
                .then(a.node.cmp(&b.node))
                .then(a.kind.cmp(&b.kind))
        });
        slots.truncate(MAX_SLOTS);
        slots
    }

    /// Plan a job across the pool.
    pub fn plan(
        &self,
        spec: JobSpec,
        coordinator: NodeId,
        context: &PlanContext<'_>,
        now_ms: u64,
    ) -> Result<JobPlan> {
        spec.validate()?;
        let mut slots = self.slots(&spec, &coordinator, context, now_ms);
        let mut distinct: Vec<NodeId> = slots.iter().map(|s| s.node).collect();
        distinct.sort();
        distinct.dedup();
        let most = u32::from(spec.redundancy) + u32::from(spec.spot_check_milli > 0);
        if distinct.len() < usize::from(spec.redundancy) {
            return Err(DarmError::Rejected(format!(
                "need {} distinct eligible nodes, pool has {}",
                spec.redundancy,
                distinct.len()
            )));
        }
        // Spot checks need one more distinct node; without it they are
        // skipped for this plan rather than placed on a replica's node.
        let spot_possible = distinct.len() as u32 >= most;
        let mut assignments = Vec::with_capacity(spec.inputs.len() * usize::from(spec.redundancy));
        let (mut cpu, mut gpu, mut npu) = (0u32, 0u32, 0u32);
        for shard in 0..spec.inputs.len() as u32 {
            let mut used: Vec<NodeId> = Vec::new();
            let mut backends: Vec<Backend> = Vec::new();
            let mut first_kind = None;
            let replicas = if spot_possible {
                spec.replicas(shard)
            } else {
                u32::from(spec.redundancy)
            };
            for replica in 0..replicas {
                let eligible = |s: &Slot| {
                    !used.contains(&s.node)
                        && (!spec.same_kind_replicas || first_kind.is_none_or(|k| k == s.kind))
                };
                let adjusted = |s: &Slot| {
                    if spec.diverse_replicas && backends.contains(&s.backend) {
                        s.weight * SAME_BACKEND_PENALTY
                    } else {
                        s.weight
                    }
                };
                let total: f64 = slots.iter().filter(|s| eligible(s)).map(adjusted).sum();
                if total <= 0.0 {
                    return Err(DarmError::Rejected(format!(
                        "no eligible worker for shard {shard} replica {replica}"
                    )));
                }
                // Smooth weighted round robin: every eligible slot gains its
                // (diversity-adjusted) weight; the highest wins and pays
                // back the total.
                for slot in slots.iter_mut() {
                    if eligible(slot) {
                        slot.current += adjusted(slot);
                    }
                }
                let best = slots
                    .iter()
                    .enumerate()
                    .filter(|(_, s)| eligible(s))
                    .max_by(|(ia, a), (ib, b)| a.current.total_cmp(&b.current).then(ib.cmp(ia)))
                    .map(|(i, _)| i)
                    .expect("eligible slots exist");
                slots[best].current -= total;
                let slot = &slots[best];
                used.push(slot.node);
                backends.push(slot.backend);
                first_kind.get_or_insert(slot.kind);
                match slot.kind {
                    DeviceKind::Cpu => cpu += 1,
                    DeviceKind::Gpu => gpu += 1,
                    DeviceKind::Npu => npu += 1,
                }
                assignments.push(Assignment {
                    shard,
                    replica: u8::try_from(replica).unwrap_or(u8::MAX),
                    worker: slot.node,
                    device: slot.kind,
                    backend: slot.backend,
                    mesh_peer: self
                        .members
                        .get(&slot.node)
                        .and_then(|m| m.advert.advert.mesh_peer.clone()),
                });
            }
        }
        Ok(JobPlan {
            job: spec.job_id(&coordinator, &self.network),
            coordinator,
            spec,
            assignments,
            cpu_shards: cpu,
            gpu_shards: gpu,
            npu_shards: npu,
        })
    }

    /// Routing multiplier for a mesh peer serving inference, in `[0.25, 4]`.
    /// Unknown peers are neutral (1.0). Faster and idler peers score higher;
    /// phones running on battery score lower so the mesh spares them.
    pub fn route_factor(&self, peer: &str, now_ms: u64) -> f64 {
        let Some(member) = self.node_for_peer(peer).and_then(|n| self.members.get(&n)) else {
            return 1.0;
        };
        let advert = &member.advert.advert;
        if advert.expires_ms() <= now_ms {
            return 1.0;
        }
        let best = |m: &Member| {
            m.advert
                .advert
                .devices
                .iter()
                .map(|d| f64::from(d.device.gflops) * f64::from(d.share_pct) / 100.0)
                .fold(0.0, f64::max)
        };
        let mut all: Vec<f64> = self
            .members
            .values()
            .map(best)
            .filter(|g| *g > 0.0)
            .collect();
        all.sort_by(f64::total_cmp);
        let median = all.get(all.len() / 2).copied().unwrap_or(1.0).max(1.0);
        let relative = (best(member) / median).max(0.0).sqrt();
        (relative * member.availability() * member.mobile_factor() * member.reliability() * 2.0)
            .clamp(0.25, 4.0)
    }

    pub fn summary(&self, now_ms: u64) -> PoolSummary {
        let mut summary = PoolSummary::default();
        for member in self.members.values() {
            let advert = &member.advert.advert;
            if advert.expires_ms() <= now_ms {
                continue;
            }
            summary.nodes += 1;
            if matches!(advert.form_factor, FormFactor::Phone | FormFactor::Tablet) {
                summary.mobile_nodes += 1;
            }
            for offered in &advert.devices {
                let d = &offered.device;
                let gflops = u64::from(d.gflops) * u64::from(offered.share_pct) / 100;
                match d.kind {
                    DeviceKind::Cpu => {
                        summary.cpu_devices += 1;
                        summary.cpu_cores += u64::from(d.compute_units);
                        summary.cpu_gflops += gflops;
                    }
                    DeviceKind::Gpu | DeviceKind::Npu => {
                        if d.kind == DeviceKind::Gpu {
                            summary.gpu_devices += 1;
                        } else {
                            summary.npu_devices += 1;
                        }
                        summary.accelerator_memory_mb += u64::from(d.memory_mb);
                        summary.accelerator_gflops += gflops;
                    }
                }
            }
        }
        summary
    }
}

#[cfg(test)]
mod tests;
