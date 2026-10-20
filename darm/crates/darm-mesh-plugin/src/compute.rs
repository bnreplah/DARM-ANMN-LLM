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
    pub fn new(config: &DarmConfig, network: Hash32) -> Self {
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
            pool: ComputePool::new(network),
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
}
