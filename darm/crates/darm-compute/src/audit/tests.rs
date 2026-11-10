use super::*;
use crate::admission::Threshold;
use crate::device::{Backend, DeviceKind, WorkloadKind};
use crate::pool::{Assignment, DevicePreference, JobSpec, SyncClass};
use crate::receipt::{WorkReceipt, tally};
use darm_core::NodeIdentity;

const SHARDS: u32 = 200;

fn id(seed: u8) -> NodeIdentity {
    NodeIdentity::from_secret_bytes(&[seed; 32])
}

fn h(tag: &str, n: u32) -> Hash32 {
    Hash32::digest("audit-test", format!("{tag}{n}").as_bytes())
}

/// A job of `SHARDS` shards, one replica each, all on worker 1.
fn plan() -> JobPlan {
    let spec = JobSpec {
        workload: WorkloadKind::Embedding,
        device: DevicePreference::Any,
        inputs: (0..SHARDS).map(|s| h("in", s)).collect(),
        redundancy: 1,
        min_memory_mb: 0,
        allow_mobile: false,
        same_kind_replicas: false,
        threshold: Threshold::TWO_THIRDS,
        spot_check_milli: 0,
        diverse_replicas: true,
        sync_class: SyncClass::Async,
        nonce: 1,
    };
    JobPlan {
        job: h("job", 0),
        coordinator: id(9).node_id(),
        assignments: (0..SHARDS)
            .map(|shard| Assignment {
                shard,
                replica: 0,
                worker: id(1).node_id(),
                device: DeviceKind::Cpu,
                backend: Backend::Cpu,
                mesh_peer: None,
            })
            .collect(),
        spec,
        cpu_shards: SHARDS,
        gpu_shards: 0,
        npu_shards: 0,
    }
}

fn receipt(plan: &JobPlan, who: &NodeIdentity, shard: u32, output: Hash32) -> SignedReceipt {
    SignedReceipt::sign(
        who,
        WorkReceipt {
            job: plan.job,
            shard,
            worker: who.node_id(),
            input: plan.spec.inputs[shard as usize],
            output,
            device: DeviceKind::Cpu,
            backend: Backend::Cpu,
            units: 1,
            elapsed_ms: 1,
            completed_ms: 1,
        },
    )
    .unwrap()
}

/// The worker's receipts; shards in `wrong` carry a bad output.
fn worked(plan: &JobPlan, wrong: &[u32]) -> Tally {
    let receipts: Vec<SignedReceipt> = (0..SHARDS)
        .map(|s| {
            let output = if wrong.contains(&s) {
                h("bad", s)
            } else {
                h("out", s)
            };
            receipt(plan, &id(1), s, output)
        })
        .collect();
    let tally = tally(plan, &receipts);
    assert!(tally.complete);
    tally
}

fn candidates() -> Vec<NodeId> {
    (1..=6).map(|s| id(s).node_id()).collect()
}

const POLICY: AuditPolicy = AuditPolicy { rate_milli: 100 };

/// Honest auditors: re-run and report the true output.
fn honest_audit(plan: &JobPlan, audit: &AuditPlan) -> Vec<SignedReceipt> {
    audit
        .assignments
        .iter()
        .map(|a| {
            let who = (1..=6).map(id).find(|i| i.node_id() == a.auditor).unwrap();
            receipt(plan, &who, a.shard, h("out", a.shard))
        })
        .collect()
}

#[test]
fn the_sample_is_reproducible_and_near_the_rate() {
    let plan = plan();
    let tally = worked(&plan, &[]);
    let beacon = h("beacon", 1);
    let a = plan_audit(&plan, &tally, &beacon, POLICY, &candidates()).unwrap();
    let mut shuffled = candidates();
    shuffled.reverse();
    let b = plan_audit(&plan, &tally, &beacon, POLICY, &shuffled).unwrap();
    assert_eq!(a, b, "anyone with the same inputs computes the same audit");
    // 10% of 200 shards is 20; allow sampling spread.
    assert!(
        (8..=36).contains(&a.assignments.len()),
        "{}",
        a.assignments.len()
    );
    assert!(a.unassignable.is_empty());
}

#[test]
fn a_worker_never_audits_its_own_shard() {
    let plan = plan();
    let tally = worked(&plan, &[]);
    for n in 0..20 {
        let audit = plan_audit(&plan, &tally, &h("beacon", n), POLICY, &candidates()).unwrap();
        assert!(
            audit
                .assignments
                .iter()
                .all(|a| a.auditor != id(1).node_id())
        );
    }
    // With the worker as the only candidate, sampled shards are reported
    // as unassignable instead of being quietly skipped.
    let only_worker = [id(1).node_id()];
    let audit = plan_audit(&plan, &tally, &h("beacon", 0), POLICY, &only_worker).unwrap();
    assert!(audit.assignments.is_empty());
    assert!(!audit.unassignable.is_empty());
    assert!(!audit_tally(&plan, &audit, &[]).clean);
}

#[test]
fn the_sample_depends_on_the_beacon_and_on_the_receipts() {
    let plan = plan();
    let honest = worked(&plan, &[]);
    let shards =
        |audit: &AuditPlan| -> Vec<u32> { audit.assignments.iter().map(|a| a.shard).collect() };
    let a = plan_audit(&plan, &honest, &h("beacon", 1), POLICY, &candidates()).unwrap();
    let b = plan_audit(&plan, &honest, &h("beacon", 2), POLICY, &candidates()).unwrap();
    assert_ne!(shards(&a), shards(&b), "a new beacon draws a new sample");
    // Changing any receipt changes the root, and with it the sample, so a
    // worker cannot keep a sample it likes while altering its results.
    let altered = worked(&plan, &[7]);
    let c = plan_audit(&plan, &altered, &h("beacon", 1), POLICY, &candidates()).unwrap();
    assert_ne!(shards(&a), shards(&c));
}

#[test]
fn honest_work_audits_clean() {
    let plan = plan();
    let tally = worked(&plan, &[]);
    let audit = plan_audit(&plan, &tally, &h("beacon", 3), POLICY, &candidates()).unwrap();
    let outcome = audit_tally(&plan, &audit, &honest_audit(&plan, &audit));
    assert!(outcome.clean);
    assert_eq!(outcome.rejected, 0);
    assert_eq!(outcome.contradicted().count(), 0);
}

#[test]
fn cheating_is_caught_at_about_the_predicted_rate() {
    // The worker returns wrong results on 10 of 200 shards. At a 10% audit
    // rate the chance of catching at least one is 1 - 0.9^10, about 65%.
    let plan = plan();
    let wrong: Vec<u32> = (0..10).map(|i| i * 17 + 3).collect();
    let tally = worked(&plan, &wrong);
    let trials = 3_000;
    let mut sampled = 0;
    for n in 0..trials {
        let audit = plan_audit(&plan, &tally, &h("beacon", n), POLICY, &candidates()).unwrap();
        if audit.assignments.iter().any(|a| wrong.contains(&a.shard)) {
            sampled += 1;
        }
    }
    let observed = f64::from(sampled) / f64::from(trials);
    let predicted = detection_probability(POLICY.rate_milli, 10);
    assert!((predicted - 0.651).abs() < 0.001);
    // Three standard errors at 3,000 trials is about 0.026.
    assert!(
        (observed - predicted).abs() < 0.03,
        "observed {observed}, predicted {predicted}"
    );
}

#[test]
fn a_sampled_wrong_shard_is_contradicted_and_only_that_shard() {
    let plan = plan();
    let wrong: Vec<u32> = (0..10).map(|i| i * 17 + 3).collect();
    let tally = worked(&plan, &wrong);
    let mut caught = 0;
    for n in 0..12 {
        let audit = plan_audit(&plan, &tally, &h("beacon", n), POLICY, &candidates()).unwrap();
        let outcome = audit_tally(&plan, &audit, &honest_audit(&plan, &audit));
        let hits: Vec<u32> = outcome.contradicted().collect();
        let expected: Vec<u32> = audit
            .assignments
            .iter()
            .map(|a| a.shard)
            .filter(|s| wrong.contains(s))
            .collect();
        assert_eq!(hits, expected);
        assert_eq!(outcome.clean, hits.is_empty());
        caught += usize::from(!hits.is_empty());
    }
    assert!(
        caught > 0,
        "at least one of these samples hits a wrong shard"
    );
}

#[test]
fn detection_probability_matches_the_formula() {
    assert_eq!(detection_probability(100, 0), 0.0);
    assert!((detection_probability(100, 1) - 0.1).abs() < 1e-12);
    assert!((detection_probability(50, 50) - (1.0 - 0.95f64.powi(50))).abs() < 1e-12);
    assert_eq!(detection_probability(1_000, 1), 1.0);
}

#[test]
fn only_the_assigned_auditor_counts() {
    let plan = plan();
    let tally = worked(&plan, &[]);
    let audit = plan_audit(&plan, &tally, &h("beacon", 4), POLICY, &candidates()).unwrap();
    let first = &audit.assignments[0];
    let outsider = id(50);
    let wrong_node = (2..=6)
        .map(id)
        .find(|i| i.node_id() != first.auditor)
        .unwrap();
    let auditor = (1..=6)
        .map(id)
        .find(|i| i.node_id() == first.auditor)
        .unwrap();
    let mut forged = receipt(&plan, &auditor, first.shard, h("out", first.shard));
    forged.receipt.output = h("bad", 0);
    let mut other_input = receipt(&plan, &auditor, first.shard, h("out", first.shard));
    other_input.receipt.input = h("in", 9_999);
    let receipts = [
        receipt(&plan, &outsider, first.shard, h("bad", 0)),
        receipt(&plan, &wrong_node, first.shard, h("bad", 0)),
        forged,
        other_input,
    ];
    let outcome = audit_tally(&plan, &audit, &receipts);
    assert_eq!(outcome.rejected, 4);
    assert_eq!(outcome.verdicts[0], (first.shard, AuditVerdict::Missing));
    assert!(!outcome.clean, "a missing audit is not a clean audit");
}

#[test]
fn a_contradiction_is_settled_by_more_auditors_not_by_either_side() {
    let plan = plan();
    let tally = worked(&plan, &[]);
    let audit = plan_audit(&plan, &tally, &h("beacon", 5), POLICY, &candidates()).unwrap();
    let first = audit.assignments[0].clone();
    let shard = first.shard;
    let accepted = h("out", shard);
    let escalated = escalation(&plan, &audit, shard, &candidates(), 3);
    assert_eq!(escalated.len(), 3);
    assert!(!escalated.contains(&id(1).node_id()), "not the worker");
    assert!(!escalated.contains(&first.auditor), "not the first auditor");
    assert_eq!(
        escalated,
        escalation(&plan, &audit, shard, &candidates(), 3)
    );
    let signer = |node: &NodeId| (1..=6).map(id).find(|i| i.node_id() == *node).unwrap();
    let say = |node: &NodeId, output: Hash32| receipt(&plan, &signer(node), shard, output);

    // The first auditor lied; two of three escalation auditors reproduce
    // the accepted output.
    let receipts = [say(&escalated[0], accepted), say(&escalated[1], accepted)];
    assert_eq!(
        resolve(&plan, shard, &accepted, &escalated, &receipts),
        Resolution::Upheld
    );

    // The worker lied; the escalation auditors agree on the true output.
    let truth = h("truth", shard);
    let receipts = [say(&escalated[0], truth), say(&escalated[2], truth)];
    assert_eq!(
        resolve(&plan, shard, &accepted, &escalated, &receipts),
        Resolution::Overturned { output: truth }
    );

    // One answer of three, a split, or votes from nodes that were not
    // escalated to, decide nothing.
    let one = [say(&escalated[0], accepted)];
    assert_eq!(
        resolve(&plan, shard, &accepted, &escalated, &one),
        Resolution::Unresolved
    );
    let split = [say(&escalated[0], accepted), say(&escalated[1], truth)];
    assert_eq!(
        resolve(&plan, shard, &accepted, &escalated, &split),
        Resolution::Unresolved
    );
    let uninvited = [say(&id(1).node_id(), truth), say(&first.auditor, truth)];
    assert_eq!(
        resolve(&plan, shard, &accepted, &escalated, &uninvited),
        Resolution::Unresolved
    );
    // Repeating one auditor's vote does not make a majority.
    let repeated = [say(&escalated[0], truth), say(&escalated[0], truth)];
    assert_eq!(
        resolve(&plan, shard, &accepted, &escalated, &repeated),
        Resolution::Unresolved
    );
}

#[test]
fn incomplete_or_mismatched_tallies_cannot_be_audited() {
    let plan = plan();
    let partial = tally(&plan, &[receipt(&plan, &id(1), 0, h("out", 0))]);
    assert!(!partial.complete);
    assert!(plan_audit(&plan, &partial, &h("beacon", 0), POLICY, &candidates()).is_err());
    let mut other = worked(&plan, &[]);
    other.job = h("job", 1);
    assert!(plan_audit(&plan, &other, &h("beacon", 0), POLICY, &candidates()).is_err());
    let complete = worked(&plan, &[]);
    for rate_milli in [0, 1_001] {
        let policy = AuditPolicy { rate_milli };
        assert!(plan_audit(&plan, &complete, &h("beacon", 0), policy, &candidates()).is_err());
    }
}
