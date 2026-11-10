//! This node's place in the compute pool: what it offers, the adverts it
//! signs, the pool of peers' adverts, and the jobs it coordinates.

use crate::config::DarmConfig;
use darm_compute::advert::{MAX_TTL_MS, MeshBinding};
use darm_compute::{
    CapabilityAdvert, ComputePool, ComputeProfile, Conditions, JobPlan, ShardSizer, SignedAdvert,
    Thermal,
};
use darm_core::{Hash32, NodeIdentity, Result};
use darm_protocol::DeviceHints;
use serde_json::{Value, json};
use std::collections::VecDeque;

pub const ADVERT_INTERVAL_MS: u64 = 60_000;
const MAX_PLANS: usize = 64;

pub struct ComputeState {
    pub enabled: bool,
    pub profile: ComputeProfile,
    pub conditions: Conditions,
    pub pool: ComputePool,
    pub sizer: ShardSizer,
    /// This node's mesh endpoint, and the host's binding of our DARM key to
    /// it, once the host has answered the plugin-key request.
    pub mesh_peer: Option<String>,
    pub mesh_binding: Option<MeshBinding>,
    pub last_advert: Option<SignedAdvert>,
    seq: u64,
    last_advert_ms: u64,
    dirty: bool,
    plans: VecDeque<JobPlan>,
}

impl ComputeState {
    pub fn new(config: &DarmConfig, network: Hash32, local: darm_core::NodeId) -> Self {
        let conditions = Conditions {
            metered: config.metered,
            on_battery: false,
            ..Conditions::default()
        };
        Self {
            enabled: config.compute_pool,
            profile: ComputeProfile::detect(
                config.form_factor,
                config.gpus.clone(),
                config.workloads.clone(),
            ),
            conditions,
            pool: ComputePool::new(network)
                .with_min_age(config.pool_min_age_ms)
                .with_local(local),
            sizer: ShardSizer::default(),
            mesh_peer: None,
            mesh_binding: None,
            last_advert: None,
            seq: 0,
            last_advert_ms: 0,
            dirty: true,
            plans: VecDeque::new(),
        }
    }

    /// Device hints for the adaptive protocol layer.
    pub fn hints(&self) -> DeviceHints {
        DeviceHints {
            mobile: self.profile.form_factor.is_mobile(),
            on_battery: self.conditions.on_battery && !self.conditions.charging,
            battery_pct: self.conditions.battery_pct,
            metered: self.conditions.metered,
            hot: self.conditions.thermal >= Thermal::Serious,
        }
    }

    pub fn set_conditions(&mut self, conditions: Conditions) {
        self.conditions = conditions;
        self.dirty = true;
    }

    pub fn set_mesh_binding(&mut self, endpoint: String, binding: Option<MeshBinding>) {
        self.mesh_peer = Some(endpoint);
        self.mesh_binding = binding;
        self.dirty = true;
    }

    /// Sign a fresh advert when one is due (periodically, slower on
    /// constrained devices, or right away after conditions changed).
    pub fn advert_due(
        &mut self,
        identity: &NodeIdentity,
        config: &DarmConfig,
        network: Hash32,
        slowdown: u64,
        now_ms: u64,
    ) -> Result<Option<SignedAdvert>> {
        if !self.enabled {
            return Ok(None);
        }
        let interval = ADVERT_INTERVAL_MS.saturating_mul(slowdown.max(1));
        if !self.dirty && now_ms.saturating_sub(self.last_advert_ms) < interval {
            return Ok(None);
        }
        // Strictly increasing across restarts without persisting a counter.
        self.seq = now_ms.max(self.seq + 1);
        let advert = CapabilityAdvert {
            node: identity.node_id(),
            network,
            seq: self.seq,
            issued_ms: now_ms,
            ttl_ms: interval.saturating_mul(3).min(MAX_TTL_MS),
            form_factor: self.profile.form_factor,
            devices: self.profile.offer(&config.policy, &self.conditions),
            workloads: self.profile.workloads.clone(),
            conditions: self.conditions,
            mesh_peer: self.mesh_peer.clone(),
            mesh_binding: self.mesh_binding.clone(),
        };
        let signed = SignedAdvert::sign(identity, advert)?;
        self.pool.upsert(signed.clone(), None, true, now_ms)?;
        self.last_advert_ms = now_ms;
        self.last_advert = Some(signed.clone());
        self.dirty = false;
        Ok(Some(signed))
    }

    pub fn remember_plan(&mut self, plan: JobPlan) {
        self.plans.retain(|p| p.job != plan.job);
        if self.plans.len() >= MAX_PLANS {
            self.plans.pop_front();
        }
        self.plans.push_back(plan);
    }

    pub fn plan(&self, job: &Hash32) -> Option<&JobPlan> {
        self.plans.iter().find(|p| p.job == *job)
    }

    pub fn status(&self, config: &DarmConfig, now_ms: u64) -> Value {
        json!({
            "enabled": self.enabled,
            "profile": self.profile,
            "policy": config.policy,
            "conditions": self.conditions,
            "contributing": self.profile.offer(&config.policy, &self.conditions),
            "blocked_by": config.policy.blocked_by(&self.conditions),
            "mesh_peer": self.mesh_peer,
            "mesh_binding_valid": self.mesh_binding.is_some(),
            "pool": self.pool.summary(now_ms),
            "plans": self.plans.iter().map(|p| json!({
                "job": p.job,
                "shards": p.spec.inputs.len(),
                "redundancy": p.spec.redundancy,
                "cpu_shards": p.cpu_shards,
                "gpu_shards": p.gpu_shards,
                "npu_shards": p.npu_shards,
            })).collect::<Vec<_>>(),
        })
    }

    /// Operator-facing resource view: how many nodes, how much CPU, GPU/NPU
    /// and accelerator memory the pool offers right now (`available`), how
    /// much is committed to in-flight jobs (`allocated`), and what this node
    /// itself contributes (`local`). `status` reports the same facts spread
    /// across `pool` and `plans`; this reshapes them into the capacity /
    /// in-use split an operator reads when deciding whether to add nodes.
    pub fn resources(&self, config: &DarmConfig, now_ms: u64) -> Value {
        let pool = self.pool.summary(now_ms);

        // In-flight allocation: shards dealt to each device kind across the
        // jobs this node is coordinating, and the distinct workers holding
        // them (a worker counts once even when it holds several shards).
        let mut busy_workers = std::collections::BTreeSet::new();
        let (mut cpu_shards, mut gpu_shards, mut npu_shards) = (0u64, 0u64, 0u64);
        for plan in &self.plans {
            cpu_shards += u64::from(plan.cpu_shards);
            gpu_shards += u64::from(plan.gpu_shards);
            npu_shards += u64::from(plan.npu_shards);
            for assignment in &plan.assignments {
                busy_workers.insert(assignment.worker);
            }
        }

        // This node's own contribution, so a standalone node sees its local
        // CPU/GPU even before any peer advert has arrived.
        let contributing = self.profile.offer(&config.policy, &self.conditions);
        let (mut local_cpu_cores, mut local_accel_mem, mut local_gflops) = (0u64, 0u64, 0u64);
        for device in &self.profile.devices {
            local_gflops += u64::from(device.gflops);
            match device.kind {
                darm_compute::DeviceKind::Cpu => local_cpu_cores += u64::from(device.compute_units),
                _ => local_accel_mem += u64::from(device.memory_mb),
            }
        }

        json!({
            "enabled": self.enabled,
            "available": {
                "nodes": pool.nodes,
                "mobile_nodes": pool.mobile_nodes,
                "cpu_devices": pool.cpu_devices,
                "cpu_cores": pool.cpu_cores,
                "gpu_devices": pool.gpu_devices,
                "npu_devices": pool.npu_devices,
                "accelerator_memory_mb": pool.accelerator_memory_mb,
                "cpu_gflops": pool.cpu_gflops,
                "accelerator_gflops": pool.accelerator_gflops,
            },
            "allocated": {
                "jobs": self.plans.len(),
                "busy_workers": busy_workers.len(),
                "cpu_shards": cpu_shards,
                "gpu_shards": gpu_shards,
                "npu_shards": npu_shards,
            },
            "local": {
                "form_factor": self.profile.form_factor,
                "contributing": contributing,
                "blocked_by": config.policy.blocked_by(&self.conditions),
                "cpu_cores": local_cpu_cores,
                "accelerator_memory_mb": local_accel_mem,
                "gflops": local_gflops,
                "devices": self.profile.devices,
            },
        })
    }
}
