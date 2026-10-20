//! Compute-pool operations (MCP tools / plugin HTTP), run with the local
//! operator's authority like the other `darm_*` operations.
//!
//! - `darm_compute_status`: offer, conditions, pool summary, sessions.
//! - `darm_compute_conditions`: report live device state (a mobile app
//!   calls this when the phone is plugged in, goes on cellular or warms up).
//! - `darm_compute_plan`: split a job across CPU/GPU/NPU workers.
//! - `darm_compute_receipt`: sign a receipt for work this node did.
//! - `darm_compute_tally`: verify receipts for a job this node planned,
//!   feed measurements back into the scheduler, optionally commit the
//!   result to the ledger.

use crate::{SharedNode, send_outbound};
use darm_compute::{
    Backend, Conditions, DeviceKind, DevicePreference, JobSpec, SignedReceipt, Thermal,
    WorkReceipt, WorkloadKind, tally,
};
use darm_core::Hash32;
use darm_core::time::now_ms;
use mesh_llm_plugin::{
    ManifestEntry, OperationRouter, PluginError, json_schema_operation, operation,
};
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::{Value, json};
use std::sync::Arc;

const MAX_RECEIPTS: usize = 4_096 * 7;

#[derive(Debug, Deserialize, JsonSchema, Default)]
pub struct StatusArgs {}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ConditionsArgs {
    #[serde(default)]
    pub on_battery: Option<bool>,
    #[serde(default)]
    pub charging: Option<bool>,
    /// 0..=100.
    #[serde(default)]
    pub battery_pct: Option<u8>,
    #[serde(default)]
    pub metered: Option<bool>,
    /// nominal | fair | serious | critical.
    #[serde(default)]
    pub thermal: Option<String>,
    /// 0.0..=1.0 current load.
    #[serde(default)]
    pub load: Option<f32>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct PlanArgs {
    /// inference | embedding | indexing | training | verification.
    pub workload: String,
    /// any | cpu_only | gpu_only | prefer_gpu | prefer_cpu (default any).
    #[serde(default)]
    pub device: Option<String>,
    /// Hex hash of each shard's input, 1..=4096.
    pub inputs: Vec<String>,
    /// Workers per shard, 1..=7 (default 3).
    #[serde(default)]
    pub redundancy: Option<u8>,
    #[serde(default)]
    pub min_memory_mb: Option<u32>,
    #[serde(default)]
    pub allow_mobile: bool,
    #[serde(default)]
    pub same_kind_replicas: bool,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ReceiptArgs {
    pub job: String,
    pub shard: u32,
    pub input: String,
    /// Hex hash of the workload's canonical output encoding.
    pub output: String,
    /// cpu | gpu | npu.
    pub device: String,
    /// cpu | cuda | rocm | metal | vulkan | other.
    pub backend: String,
    pub units: u32,
    pub elapsed_ms: u32,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct TallyArgs {
    pub job: String,
    /// Signed receipts as returned by `darm_compute_receipt`.
    pub receipts: Vec<Value>,
    /// Commit the verified result to the ledger (default false).
    #[serde(default)]
    pub commit: bool,
}

const STATUS_DESC: &str =
    "Compute pool: this node's CPU/GPU offer, live conditions, pool totals and sessions";
const CONDITIONS_DESC: &str =
    "Report live device conditions (battery, charging, metered network, thermal, load)";
const PLAN_DESC: &str = "Plan a job across pooled CPUs, GPUs and NPUs with redundant replicas";
const RECEIPT_DESC: &str = "Sign a work receipt for a shard this node computed";
const TALLY_DESC: &str = "Verify receipts for a planned job by majority, update measurements, optionally commit to the ledger";

pub fn manifest_entries() -> Vec<ManifestEntry> {
    vec![
        operation::<StatusArgs>("darm_compute_status", STATUS_DESC).into(),
        operation::<ConditionsArgs>("darm_compute_conditions", CONDITIONS_DESC).into(),
        operation::<PlanArgs>("darm_compute_plan", PLAN_DESC).into(),
        operation::<ReceiptArgs>("darm_compute_receipt", RECEIPT_DESC).into(),
        operation::<TallyArgs>("darm_compute_tally", TALLY_DESC).into(),
    ]
}

fn bad(error: impl std::fmt::Display) -> PluginError {
    PluginError::invalid_params(error.to_string())
}

fn hash(value: &str) -> Result<Hash32, PluginError> {
    Hash32::from_hex(value).map_err(bad)
}

fn parse_thermal(value: &str) -> Result<Thermal, PluginError> {
    match value.trim().to_ascii_lowercase().as_str() {
        "nominal" => Ok(Thermal::Nominal),
        "fair" => Ok(Thermal::Fair),
        "serious" => Ok(Thermal::Serious),
        "critical" => Ok(Thermal::Critical),
        _ => Err(bad("thermal must be nominal|fair|serious|critical")),
    }
}

fn parse_preference(value: Option<&str>) -> Result<DevicePreference, PluginError> {
    Ok(
        match value.map(|v| v.trim().to_ascii_lowercase()).as_deref() {
            None | Some("any") => DevicePreference::Any,
            Some("cpu_only") => DevicePreference::CpuOnly,
            Some("gpu_only") => DevicePreference::GpuOnly,
            Some("prefer_gpu") => DevicePreference::PreferGpu,
            Some("prefer_cpu") => DevicePreference::PreferCpu,
            Some(_) => return Err(bad("unknown device preference")),
        },
    )
}

fn parse_device(value: &str) -> Result<DeviceKind, PluginError> {
    match value.trim().to_ascii_lowercase().as_str() {
        "cpu" => Ok(DeviceKind::Cpu),
        "gpu" => Ok(DeviceKind::Gpu),
        "npu" => Ok(DeviceKind::Npu),
        _ => Err(bad("device must be cpu|gpu|npu")),
    }
}

pub fn conditions_from(
    args: ConditionsArgs,
    current: Conditions,
) -> Result<Conditions, PluginError> {
    if args.battery_pct.is_some_and(|p| p > 100) {
        return Err(bad("battery_pct must be 0..=100"));
    }
    let load_milli = match args.load {
        Some(load) if (0.0..=1.0).contains(&load) => (load * 1_000.0).round() as u16,
        Some(_) => return Err(bad("load must be 0..=1")),
        None => current.load_milli,
    };
    Ok(Conditions {
        on_battery: args.on_battery.unwrap_or(current.on_battery),
        charging: args.charging.unwrap_or(current.charging),
        battery_pct: args.battery_pct.unwrap_or(current.battery_pct),
        metered: args.metered.unwrap_or(current.metered),
        thermal: match args.thermal.as_deref() {
            Some(t) => parse_thermal(t)?,
            None => current.thermal,
        },
        load_milli,
    })
}

pub fn register(router: &mut OperationRouter, node: &SharedNode) {
    let n = Arc::clone(node);
    router.add_json(
        json_schema_operation::<StatusArgs>("darm_compute_status", STATUS_DESC),
        move |_args: StatusArgs, _context| {
            let n = Arc::clone(&n);
            Box::pin(async move {
                let node = n.lock().expect("darm node lock");
                Ok(json!({
                    "compute": node.compute.status(&node.config, now_ms()),
                    "sessions": node.sessions.status(),
                }))
            })
        },
    );

    let n = Arc::clone(node);
    router.add_json(
        json_schema_operation::<ConditionsArgs>("darm_compute_conditions", CONDITIONS_DESC),
        move |args: ConditionsArgs, _context| {
            let n = Arc::clone(&n);
            Box::pin(async move {
                let mut node = n.lock().expect("darm node lock");
                let conditions = conditions_from(args, node.compute.conditions)?;
                node.set_conditions(conditions);
                Ok(json!({
                    "conditions": conditions,
                    "blocked_by": node.config.policy.blocked_by(&conditions),
                    "tuning": node.sessions.local_tuning(),
                }))
            })
        },
    );

    let n = Arc::clone(node);
    router.add_json(
        json_schema_operation::<PlanArgs>("darm_compute_plan", PLAN_DESC),
        move |args: PlanArgs, _context| {
            let n = Arc::clone(&n);
            Box::pin(async move {
                let workload =
                    WorkloadKind::parse(&args.workload).ok_or_else(|| bad("unknown workload"))?;
                if args.inputs.is_empty() || args.inputs.len() > darm_compute::pool::MAX_PLAN_SHARDS
                {
                    return Err(bad("inputs must hold 1..=4096 hashes"));
                }
                let inputs = args
                    .inputs
                    .iter()
                    .map(|i| hash(i))
                    .collect::<Result<Vec<_>, _>>()?;
                let mut nonce = [0u8; 8];
                getrandom::fill(&mut nonce).map_err(|e| PluginError::internal(e.to_string()))?;
                let spec = JobSpec {
                    workload,
                    device: parse_preference(args.device.as_deref())?,
                    inputs,
                    redundancy: args.redundancy.unwrap_or(3),
                    min_memory_mb: args.min_memory_mb.unwrap_or(0),
                    allow_mobile: args.allow_mobile,
                    same_kind_replicas: args.same_kind_replicas,
                    nonce: u64::from_be_bytes(nonce),
                };
                let mut node = n.lock().expect("darm node lock");
                let me = node.node_id();
                let trust = |peer: &darm_core::NodeId| node.compute_trust(peer);
                let plan = node
                    .compute
                    .pool
                    .plan(spec, me, &trust, now_ms())
                    .map_err(bad)?;
                let units: Vec<Value> = plan
                    .assignments
                    .iter()
                    .map(|a| {
                        json!({
                            "shard": a.shard,
                            "replica": a.replica,
                            "worker": a.worker,
                            "mesh_peer": a.mesh_peer,
                            "device": a.device,
                            "backend": a.backend,
                            "suggested_units": node.compute.sizer.next_units(&a.worker, a.device),
                        })
                    })
                    .collect();
                let response = json!({
                    "job": plan.job,
                    "input_root": plan.spec.input_root(),
                    "cpu_shards": plan.cpu_shards,
                    "gpu_shards": plan.gpu_shards,
                    "npu_shards": plan.npu_shards,
                    "assignments": units,
                });
                node.compute.remember_plan(plan);
                Ok(response)
            })
        },
    );

    let n = Arc::clone(node);
    router.add_json(
        json_schema_operation::<ReceiptArgs>("darm_compute_receipt", RECEIPT_DESC),
        move |args: ReceiptArgs, _context| {
            let n = Arc::clone(&n);
            Box::pin(async move {
                let backend =
                    Backend::parse(&args.backend).ok_or_else(|| bad("unknown backend"))?;
                let node = n.lock().expect("darm node lock");
                let receipt = WorkReceipt {
                    job: hash(&args.job)?,
                    shard: args.shard,
                    worker: node.node_id(),
                    input: hash(&args.input)?,
                    output: hash(&args.output)?,
                    device: parse_device(&args.device)?,
                    backend,
                    units: args.units,
                    elapsed_ms: args.elapsed_ms,
                    completed_ms: now_ms(),
                };
                let signed = SignedReceipt::sign(node.identity(), receipt).map_err(bad)?;
                serde_json::to_value(signed).map_err(|e| PluginError::internal(e.to_string()))
            })
        },
    );

    let n = Arc::clone(node);
    router.add_json(
        json_schema_operation::<TallyArgs>("darm_compute_tally", TALLY_DESC),
        move |args: TallyArgs, context| {
            let n = Arc::clone(&n);
            Box::pin(async move {
                if args.receipts.len() > MAX_RECEIPTS {
                    return Err(bad("too many receipts"));
                }
                let job = hash(&args.job)?;
                let receipts = args
                    .receipts
                    .into_iter()
                    .map(serde_json::from_value::<SignedReceipt>)
                    .collect::<Result<Vec<_>, _>>()
                    .map_err(bad)?;
                let (response, outbound) = {
                    let mut node = n.lock().expect("darm node lock");
                    let plan = node
                        .compute
                        .plan(&job)
                        .cloned()
                        .ok_or_else(|| bad("unknown job; plan it on this node first"))?;
                    let result = tally(&plan, &receipts);
                    let dissenters: Vec<_> = result.dissenters.iter().map(|d| d.worker).collect();
                    for receipt in &result.accepted {
                        let r = &receipt.receipt;
                        let ok = !dissenters.contains(&r.worker);
                        node.compute
                            .pool
                            .record(&r.worker, r.device, r.units, r.elapsed_ms, ok);
                        if ok {
                            node.compute
                                .sizer
                                .observe(&r.worker, r.device, r.units, r.elapsed_ms);
                        }
                    }
                    let mut response = serde_json::to_value(&result)
                        .map_err(|e| PluginError::internal(e.to_string()))?;
                    let mut outbound = Vec::new();
                    if args.commit {
                        let body = result
                            .commit_body()
                            .ok_or_else(|| bad("not every shard is agreed; nothing to commit"))?;
                        let (entry, out) = node.commit_compute(body, now_ms()).map_err(bad)?;
                        response["ledger_entry"] = json!(entry);
                        outbound = out;
                    }
                    (response, outbound)
                };
                send_outbound(context, &n, outbound)
                    .await
                    .map_err(|e| PluginError::internal(e.to_string()))?;
                Ok(response)
            })
        },
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn conditions_merge_and_validate() {
        let current = Conditions::default();
        let next = conditions_from(
            ConditionsArgs {
                on_battery: Some(true),
                charging: None,
                battery_pct: Some(40),
                metered: Some(true),
                thermal: Some("fair".into()),
                load: Some(0.25),
            },
            current,
        )
        .unwrap();
        assert!(next.on_battery && next.metered && !next.charging);
        assert_eq!((next.battery_pct, next.load_milli), (40, 250));
        assert_eq!(next.thermal, Thermal::Fair);
        let bad_pct = ConditionsArgs {
            on_battery: None,
            charging: None,
            battery_pct: Some(101),
            metered: None,
            thermal: None,
            load: None,
        };
        assert!(conditions_from(bad_pct, current).is_err());
        assert!(parse_thermal("lava").is_err());
        assert!(parse_preference(Some("quantum")).is_err());
    }
}
