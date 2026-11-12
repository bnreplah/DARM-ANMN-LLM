//! Sampled-audit operations (MCP tools / plugin HTTP) for jobs this node
//! coordinated. They wire [`darm_compute::audit`] into the node:
//!
//! - `darm_compute_audit_plan`: after a job is committed on the ledger and a
//!   later block is finalized, derive the audited shards and their auditors
//!   from `(job, receipts_root, hash of that later block)`. Nobody picks the
//!   sample: the coordinator's `receipts_root` was fixed in the commit, and the
//!   beacon block did not exist when the workers signed.
//! - `darm_compute_audit_report`: check the auditors' receipts against the
//!   accepted outputs; contradicted shards get escalation auditors.
//! - `darm_compute_audit_resolve`: settle a contradicted shard by majority of
//!   its escalation auditors and feed the outcome into worker trust.
//!
//! Limits (see `darm_compute::audit`): the beacon's proposer can bias it a
//! little, there is no stake to slash (consequence is local trust only), and
//! an overturned job is flagged here, not revoked on the ledger.

use crate::SharedNode;
use crate::compute::AuditRecord;
use crate::node::DarmNode;
use darm_compute::audit::{AuditPolicy, Resolution, audit_tally, escalation, plan_audit, resolve};
use darm_compute::{DeviceKind, SignedReceipt, tally};
use darm_core::{Hash32, NodeId};
use mesh_llm_plugin::{
    ManifestEntry, OperationRouter, PluginError, json_schema_operation, operation,
};
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::{Value, json};
use std::sync::Arc;

const MAX_RECEIPTS: usize = 4_096 * 7;
/// Auditors asked to settle one contradicted shard (odd, so a majority exists).
const ESCALATION_AUDITORS: usize = 3;

#[derive(Debug, Deserialize, JsonSchema)]
pub struct AuditPlanArgs {
    pub job: String,
    /// The workers' signed receipts, exactly as tallied for the commit.
    pub receipts: Vec<Value>,
    /// Ledger height of a finalized block that came after the job's commit;
    /// its hash is the audit beacon.
    pub beacon_height: u64,
    /// Share of shards to audit, per mille (1..=1000).
    pub rate_milli: u16,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct AuditReportArgs {
    pub job: String,
    /// Auditors' signed receipts.
    pub receipts: Vec<Value>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct AuditResolveArgs {
    pub job: String,
    pub shard: u32,
    /// Escalation auditors' signed receipts.
    pub receipts: Vec<Value>,
}

const PLAN_DESC: &str = "Choose which shards of a committed job to re-run and who audits them, from the ledger's receipts root and a later block hash";
const REPORT_DESC: &str = "Compare auditors' receipts with the accepted outputs; contradicted shards get escalation auditors";
const RESOLVE_DESC: &str =
    "Settle a contradicted shard by majority of escalation auditors and update worker trust";

pub fn manifest_entries() -> Vec<ManifestEntry> {
    vec![
        operation::<AuditPlanArgs>("darm_compute_audit_plan", PLAN_DESC).into(),
        operation::<AuditReportArgs>("darm_compute_audit_report", REPORT_DESC).into(),
        operation::<AuditResolveArgs>("darm_compute_audit_resolve", RESOLVE_DESC).into(),
    ]
}

fn bad(error: impl std::fmt::Display) -> PluginError {
    PluginError::invalid_params(error.to_string())
}

fn receipts_from(values: Vec<Value>) -> Result<Vec<SignedReceipt>, PluginError> {
    if values.len() > MAX_RECEIPTS {
        return Err(bad("too many receipts"));
    }
    values
        .into_iter()
        .map(serde_json::from_value::<SignedReceipt>)
        .collect::<Result<Vec<_>, _>>()
        .map_err(bad)
}

fn fail(node: &mut DarmNode, worker: &NodeId) {
    node.compute
        .pool
        .record(worker, DeviceKind::Cpu, 0, 0, false);
}

/// Derive the audit for a committed job and remember it.
pub fn plan(node: &mut DarmNode, args: AuditPlanArgs) -> Result<Value, PluginError> {
    let job = Hash32::from_hex(&args.job).map_err(bad)?;
    let receipts = receipts_from(args.receipts)?;
    let me = node.node_id();
    let job_plan = node
        .compute
        .plan(&job)
        .cloned()
        .ok_or_else(|| bad("unknown job; plan it on this node first"))?;
    let result = tally(&job_plan, &receipts);
    let record = node
        .ledger()
        .state()
        .compute_job(&me, &job)
        .ok_or_else(|| bad("the job has no finalized commit on the ledger"))?
        .clone();
    if !result.complete || result.receipts_root != record.receipts_root {
        return Err(bad("receipts do not match the committed receipts root"));
    }
    let beacon_block = node
        .ledger()
        .block(args.beacon_height)
        .ok_or_else(|| bad("no finalized block at beacon_height yet"))?;
    if beacon_block.header.timestamp_ms <= record.committed_ms {
        return Err(bad("the beacon block must come after the job's commit"));
    }
    let beacon = beacon_block.hash();
    let candidates = node.compute.pool.nodes();
    let audit = plan_audit(
        &job_plan,
        &result,
        &beacon,
        AuditPolicy {
            rate_milli: args.rate_milli,
        },
        &candidates,
    )
    .map_err(bad)?;
    let signers = result
        .accepted
        .iter()
        .map(|s| (s.receipt.shard, s.receipt.worker))
        .collect();
    let response = json!({
        "job": job,
        "beacon": beacon,
        "seed": audit.seed,
        "assignments": audit.assignments,
        "unassignable": audit.unassignable,
        "note": "auditors re-run the shard and sign an ordinary receipt (darm_compute_receipt)",
    });
    node.compute.remember_audit(AuditRecord {
        plan: audit,
        signers,
        escalated: Default::default(),
        resolved: Default::default(),
    });
    Ok(response)
}

/// Check auditors' receipts; open escalation for contradictions.
pub fn report(node: &mut DarmNode, args: AuditReportArgs) -> Result<Value, PluginError> {
    let job = Hash32::from_hex(&args.job).map_err(bad)?;
    let receipts = receipts_from(args.receipts)?;
    let job_plan = node
        .compute
        .plan(&job)
        .cloned()
        .ok_or_else(|| bad("unknown job"))?;
    let record = node
        .compute
        .audit(&job)
        .cloned()
        .ok_or_else(|| bad("no audit planned for this job"))?;
    let outcome = audit_tally(&job_plan, &record.plan, &receipts);
    let candidates = node.compute.pool.nodes();
    let mut escalated = Vec::new();
    for shard in outcome.contradicted().collect::<Vec<_>>() {
        let auditors = escalation(
            &job_plan,
            &record.plan,
            shard,
            &candidates,
            ESCALATION_AUDITORS,
        );
        escalated.push(json!({"shard": shard, "auditors": auditors}));
        if let Some(stored) = node.compute.audit_mut(&job) {
            stored.escalated.insert(shard, auditors);
        }
    }
    Ok(json!({
        "outcome": outcome,
        "escalation": escalated,
        "next": if outcome.clean { "clean" } else { "resolve contradicted shards, or re-audit missing ones" },
    }))
}

/// Settle one contradicted shard and update trust.
pub fn settle(node: &mut DarmNode, args: AuditResolveArgs) -> Result<Value, PluginError> {
    let job = Hash32::from_hex(&args.job).map_err(bad)?;
    let receipts = receipts_from(args.receipts)?;
    let job_plan = node
        .compute
        .plan(&job)
        .cloned()
        .ok_or_else(|| bad("unknown job"))?;
    let record = node
        .compute
        .audit(&job)
        .cloned()
        .ok_or_else(|| bad("no audit planned for this job"))?;
    let escalated = record
        .escalated
        .get(&args.shard)
        .cloned()
        .ok_or_else(|| bad("shard is not under escalation"))?;
    let assignment = record
        .plan
        .assignments
        .iter()
        .find(|a| a.shard == args.shard)
        .ok_or_else(|| bad("shard was not audited"))?;
    let accepted = assignment
        .expected
        .ok_or_else(|| bad("accepted output unknown"))?;
    let resolution = resolve(&job_plan, args.shard, &accepted, &escalated, &receipts);
    let mut penalised: Vec<NodeId> = Vec::new();
    match &resolution {
        // The first auditor contradicted a result the majority reproduced.
        Resolution::Upheld => penalised.push(assignment.auditor),
        // The accepted result was wrong: everyone who signed it is at fault.
        Resolution::Overturned { .. } => penalised.extend(
            record
                .signers
                .iter()
                .filter(|(shard, _)| *shard == args.shard)
                .map(|(_, worker)| *worker),
        ),
        Resolution::Unresolved => {}
    }
    for worker in &penalised {
        fail(node, worker);
    }
    if let Some(stored) = node.compute.audit_mut(&job) {
        stored.resolved.insert(args.shard, resolution.clone());
    }
    Ok(json!({
        "shard": args.shard,
        "resolution": resolution,
        "trust_penalised": penalised,
        "commit_stands": !matches!(resolution, Resolution::Overturned { .. }),
        "note": "an overturned job is flagged here; the ledger commit is not revoked",
    }))
}

pub fn register(router: &mut OperationRouter, node: &SharedNode) {
    let n = Arc::clone(node);
    router.add_json(
        json_schema_operation::<AuditPlanArgs>("darm_compute_audit_plan", PLAN_DESC),
        move |args: AuditPlanArgs, _context| {
            let n = Arc::clone(&n);
            Box::pin(async move {
                let mut node = n.lock().expect("darm node lock");
                plan(&mut node, args)
            })
        },
    );
    let n = Arc::clone(node);
    router.add_json(
        json_schema_operation::<AuditReportArgs>("darm_compute_audit_report", REPORT_DESC),
        move |args: AuditReportArgs, _context| {
            let n = Arc::clone(&n);
            Box::pin(async move {
                let mut node = n.lock().expect("darm node lock");
                report(&mut node, args)
            })
        },
    );
    let n = Arc::clone(node);
    router.add_json(
        json_schema_operation::<AuditResolveArgs>("darm_compute_audit_resolve", RESOLVE_DESC),
        move |args: AuditResolveArgs, _context| {
            let n = Arc::clone(&n);
            Box::pin(async move {
                let mut node = n.lock().expect("darm node lock");
                settle(&mut node, args)
            })
        },
    );
}
