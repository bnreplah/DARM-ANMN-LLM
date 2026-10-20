use super::*;
use crate::admission::Threshold;
use crate::advert::tests::{NOW, advert, network};
use crate::device::Device;
use crate::receipt::{ShardVerdict, SignedReceipt, WorkReceipt, tally};
use darm_core::NodeIdentity;
use darm_core::ledger::EntryBody;

fn identity(seed: u8) -> NodeIdentity {
    NodeIdentity::from_secret_bytes(&[seed; 32])
}

fn trust_all(_: &NodeId) -> f64 {
    1.0
}

fn no_rtt(_: &NodeId) -> Option<f64> {
    None
}

const ALL: PlanContext<'static> = PlanContext {
    trust: &trust_all,
    rtt_ms: &no_rtt,
};

fn spec(shards: usize, redundancy: u8, device: DevicePreference) -> JobSpec {
    JobSpec {
        workload: WorkloadKind::Embedding,
        device,
        inputs: (0..shards)
            .map(|i| Hash32::digest("in", &i.to_be_bytes()))
            .collect(),
        redundancy,
        min_memory_mb: 0,
        allow_mobile: true,
        same_kind_replicas: false,
        threshold: Threshold::TWO_THIRDS,
        spot_check_milli: 0,
        diverse_replicas: true,
        sync_class: SyncClass::Async,
        nonce: 7,
    }
}

/// Two CPU-only servers and one server with a GPU 10x a CPU.
fn pool() -> (ComputePool, Vec<NodeIdentity>) {
    let ids: Vec<NodeIdentity> = (1..=3).map(identity).collect();
    let mut pool = ComputePool::new(network()).with_min_age(0);
    let gpu = Device::parse_gpu("cuda:24576:1280").unwrap();
    for (i, id) in ids.iter().enumerate() {
        let gpus = if i == 2 { vec![gpu.clone()] } else { vec![] };
        let signed = advert(id, 1, gpus, FormFactor::Server);
        let peer = signed.advert.mesh_peer.clone();
        pool.upsert(signed, peer.as_deref(), false, NOW).unwrap();
    }
    (pool, ids)
}

#[test]
fn splits_work_across_cpus_and_gpus_by_weight() {
    let (pool, ids) = pool();
    let plan = pool
        .plan(
            spec(120, 1, DevicePreference::Any),
            ids[0].node_id(),
            &ALL,
            NOW,
        )
        .unwrap();
    assert_eq!(plan.assignments.len(), 120);
    // Weights: CPUs 128 each (x3), GPU 1280: GPU gets ~1280/1664 of the work.
    assert!(
        (85..=100).contains(&plan.gpu_shards),
        "gpu {}",
        plan.gpu_shards
    );
    assert!(
        plan.cpu_shards >= 20,
        "cpus still pooled: {}",
        plan.cpu_shards
    );
    let cpu_only = pool
        .plan(
            spec(30, 1, DevicePreference::CpuOnly),
            ids[0].node_id(),
            &ALL,
            NOW,
        )
        .unwrap();
    assert_eq!(cpu_only.gpu_shards, 0);
    let gpu_only = pool
        .plan(
            spec(30, 1, DevicePreference::GpuOnly),
            ids[0].node_id(),
            &ALL,
            NOW,
        )
        .unwrap();
    assert_eq!(gpu_only.cpu_shards, 0);
}

#[test]
fn replicas_land_on_distinct_nodes() {
    let (pool, ids) = pool();
    let plan = pool
        .plan(
            spec(50, 3, DevicePreference::Any),
            ids[0].node_id(),
            &ALL,
            NOW,
        )
        .unwrap();
    for shard in 0..50 {
        let mut workers: Vec<NodeId> = plan
            .assignments
            .iter()
            .filter(|a| a.shard == shard)
            .map(|a| a.worker)
            .collect();
        workers.sort();
        workers.dedup();
        assert_eq!(workers.len(), 3);
    }
    assert!(
        pool.plan(
            spec(1, 4, DevicePreference::Any),
            ids[0].node_id(),
            &ALL,
            NOW
        )
        .is_err(),
        "not enough distinct nodes"
    );
}

#[test]
fn untrusted_and_mobile_nodes_can_be_excluded() {
    let (mut pool, ids) = pool();
    let phone = identity(9);
    let signed = advert(&phone, 1, vec![], FormFactor::Phone);
    pool.upsert(signed, None, false, NOW).unwrap();
    let mut job = spec(40, 1, DevicePreference::Any);
    job.allow_mobile = false;
    let only_first = |n: &NodeId| f64::from(u8::from(*n == ids[0].node_id()));
    let context = PlanContext {
        trust: &only_first,
        rtt_ms: &no_rtt,
    };
    let plan = pool.plan(job, ids[0].node_id(), &context, NOW).unwrap();
    assert!(
        plan.assignments
            .iter()
            .all(|a| a.worker == ids[0].node_id())
    );
}

#[test]
fn adverts_replay_and_peer_binding() {
    let (mut pool, ids) = pool();
    let old = advert(&ids[0], 1, vec![], FormFactor::Server);
    assert!(
        !pool.upsert(old, None, false, NOW).unwrap(),
        "same seq ignored"
    );
    let peer0 = format!("peer{}", ids[0].node_id().short());
    assert_eq!(pool.node_for_peer(&peer0), Some(ids[0].node_id()));
    // Another node claiming peer0's endpoint from elsewhere is not mapped.
    let mut liar = advert(&ids[1], 2, vec![], FormFactor::Server).advert;
    liar.mesh_peer = Some(peer0.clone());
    let liar = SignedAdvert::sign(&ids[1], liar).unwrap();
    pool.upsert(liar.clone(), Some("somewhere-else"), false, NOW)
        .unwrap();
    assert_eq!(pool.node_for_peer(&peer0), Some(ids[0].node_id()));
    // A verified binding for peer0 cannot be displaced by an unverified one.
    let verified = advert(&ids[0], 3, vec![], FormFactor::Server);
    pool.upsert(verified, Some(&peer0), true, NOW).unwrap();
    let mut liar2 = liar.advert.clone();
    liar2.seq = 9;
    let liar2 = SignedAdvert::sign(&ids[1], liar2).unwrap();
    pool.upsert(liar2, Some(&peer0), false, NOW).unwrap();
    assert_eq!(pool.node_for_peer(&peer0), Some(ids[0].node_id()));
    // Expiry drops members and their bindings.
    pool.expire(NOW + 120_000);
    assert!(pool.is_empty());
    assert_eq!(pool.node_for_peer(&peer0), None);
}

#[test]
fn pool_capacity_is_bounded() {
    let mut pool = ComputePool::with_capacity(network(), 2).with_min_age(0);
    for seed in 1..=2 {
        pool.upsert(
            advert(&identity(seed), 1, vec![], FormFactor::Server),
            None,
            false,
            NOW,
        )
        .unwrap();
    }
    assert!(
        pool.upsert(
            advert(&identity(3), 1, vec![], FormFactor::Server),
            None,
            false,
            NOW
        )
        .is_err()
    );
}

#[test]
fn measurement_corrects_overstated_hardware() {
    let ids: Vec<NodeIdentity> = (1..=2).map(identity).collect();
    let mut pool = ComputePool::new(network()).with_min_age(0);
    // Node 1 claims a 10x GPU but delivers like node 0's GPU.
    let honest = Device::parse_gpu("cuda:8192:1000").unwrap();
    let liar = Device::parse_gpu("cuda:8192:10000").unwrap();
    pool.upsert(
        advert(&ids[0], 1, vec![honest], FormFactor::Server),
        None,
        false,
        NOW,
    )
    .unwrap();
    pool.upsert(
        advert(&ids[1], 1, vec![liar], FormFactor::Server),
        None,
        false,
        NOW,
    )
    .unwrap();
    for _ in 0..5 {
        pool.record(&ids[0].node_id(), DeviceKind::Gpu, 1_000, 1_000, true);
        pool.record(&ids[1].node_id(), DeviceKind::Gpu, 1_000, 1_000, true);
    }
    let plan = pool
        .plan(
            spec(100, 1, DevicePreference::GpuOnly),
            ids[0].node_id(),
            &ALL,
            NOW,
        )
        .unwrap();
    let liar_share = plan
        .assignments
        .iter()
        .filter(|a| a.worker == ids[1].node_id())
        .count();
    assert!((35..=65).contains(&liar_share), "liar got {liar_share}/100");
}

#[test]
fn route_factor_prefers_fast_idle_peers() {
    let (pool, ids) = pool();
    let gpu_peer = format!("peer{}", ids[2].node_id().short());
    let cpu_peer = format!("peer{}", ids[0].node_id().short());
    assert!(pool.route_factor(&gpu_peer, NOW) > pool.route_factor(&cpu_peer, NOW));
    assert_eq!(pool.route_factor("unknown", NOW), 1.0);
    let summary = pool.summary(NOW);
    assert_eq!(summary.nodes, 3);
    assert_eq!(summary.gpu_devices, 1);
    assert_eq!(summary.cpu_devices, 3);
}

fn receipt(id: &NodeIdentity, plan: &JobPlan, shard: u32, output: &[u8]) -> SignedReceipt {
    SignedReceipt::sign(
        id,
        WorkReceipt {
            job: plan.job,
            shard,
            worker: id.node_id(),
            input: plan.spec.inputs[shard as usize],
            output: Hash32::digest("out", output),
            device: DeviceKind::Cpu,
            backend: Backend::Cpu,
            units: 10,
            elapsed_ms: 100,
            completed_ms: NOW,
        },
    )
    .unwrap()
}

#[test]
fn majority_tally_flags_dissent_and_commits() {
    let (pool, ids) = pool();
    let plan = pool
        .plan(
            spec(4, 3, DevicePreference::Any),
            ids[0].node_id(),
            &ALL,
            NOW,
        )
        .unwrap();
    let mut receipts = Vec::new();
    for shard in 0..4u32 {
        for id in &ids {
            // Node 3 lies on shard 2.
            let output: &[u8] = if shard == 2 && id.node_id() == ids[2].node_id() {
                b"wrong"
            } else {
                &[shard as u8]
            };
            receipts.push(receipt(id, &plan, shard, output));
        }
    }
    // Noise: an outsider, a duplicate, and a forged signature.
    let outsider = identity(42);
    receipts.push(receipt(&outsider, &plan, 0, b"x"));
    receipts.push(receipts[0].clone());
    let mut forged = receipts[1].clone();
    forged.receipt.output = Hash32::digest("out", b"forged");
    receipts.push(forged);

    let result = tally(&plan, &receipts);
    assert!(result.complete);
    assert_eq!(result.rejected, 2, "outsider + forged");
    assert_eq!(result.dissenters.len(), 1);
    assert_eq!(result.dissenters[0].worker, ids[2].node_id());
    assert_eq!(result.dissenters[0].shard, 2);
    let Some(EntryBody::ComputeCommit {
        shards, workers, ..
    }) = result.commit_body()
    else {
        panic!("complete tally commits")
    };
    assert_eq!((shards, workers), (4, 3));

    // Without a majority nothing is committed.
    let split: Vec<SignedReceipt> = ids
        .iter()
        .enumerate()
        .map(|(i, id)| receipt(id, &plan, 0, &[i as u8]))
        .collect();
    let result = tally(&plan, &split);
    assert!(!result.complete);
    assert!(matches!(result.shards[0], ShardVerdict::Disputed { .. }));
    assert!(matches!(
        result.shards[1],
        ShardVerdict::Missing { received: 0 }
    ));
    assert!(result.commit_body().is_none());
}

#[test]
fn job_ids_commit_to_coordinator_and_parameters() {
    let a = identity(1).node_id();
    let b = identity(2).node_id();
    let job = spec(3, 1, DevicePreference::Any);
    assert_ne!(job.job_id(&a, &network()), job.job_id(&b, &network()));
    let mut other = job.clone();
    other.redundancy = 2;
    assert_ne!(job.job_id(&a, &network()), other.job_id(&a, &network()));
    assert!(spec(0, 1, DevicePreference::Any).validate().is_err());
    assert!(
        spec(MAX_PLAN_SHARDS + 1, 1, DevicePreference::Any)
            .validate()
            .is_err()
    );
    assert!(spec(1, 0, DevicePreference::Any).validate().is_err());
    assert!(
        spec(1, MAX_REDUNDANCY + 1, DevicePreference::Any)
            .validate()
            .is_err()
    );
}

#[test]
fn threshold_is_the_admission_lever() {
    let (pool, ids) = pool();
    let mut job = spec(2, 3, DevicePreference::Any);
    job.threshold = Threshold::STRICT;
    let plan = pool.plan(job, ids[0].node_id(), &ALL, NOW).unwrap();
    // Two of three agree: enough at 2/3, not at 0.9 (needs 3 of 3).
    let receipts: Vec<SignedReceipt> = plan
        .assignments
        .iter()
        .map(|a| {
            let id = ids.iter().find(|i| i.node_id() == a.worker).unwrap();
            let out: &[u8] = if a.replica == 2 {
                b"other"
            } else {
                &[a.shard as u8]
            };
            receipt(id, &plan, a.shard, out)
        })
        .collect();
    let strict = tally(&plan, &receipts);
    assert!(!strict.complete, "0.9 requires unanimity of 3");
    let mut lenient = plan.clone();
    lenient.spec.threshold = Threshold::TWO_THIRDS;
    // (Re-tally the same receipts under 2/3: the job id is unchanged here
    // because only the tally rule is being compared.)
    let result = tally(&lenient, &receipts);
    assert!(result.complete);
    assert_eq!(result.dissenters.len(), 2);
}

#[test]
fn spot_checks_add_a_hidden_replica() {
    let (pool, ids) = pool();
    let mut job = spec(200, 1, DevicePreference::Any);
    job.spot_check_milli = 250;
    let plan = pool.plan(job.clone(), ids[0].node_id(), &ALL, NOW).unwrap();
    let checked = plan.spot_checked_shards();
    assert!((25..=75).contains(&checked), "~25% of 200: {checked}");
    for shard in 0..200 {
        assert_eq!(plan.replicas(shard), job.replicas(shard));
    }
    // A single-replica shard is admitted on one receipt; a spot-checked one
    // needs both copies to agree, so a lying worker is caught there.
    let spot = (0..200u32).find(|s| plan.replicas(*s) == 2).unwrap();
    let workers: Vec<&NodeIdentity> = plan
        .assignments
        .iter()
        .filter(|a| a.shard == spot)
        .map(|a| ids.iter().find(|i| i.node_id() == a.worker).unwrap())
        .collect();
    let receipts = vec![
        receipt(workers[0], &plan, spot, b"honest"),
        receipt(workers[1], &plan, spot, b"lie"),
    ];
    let result = tally(&plan, &receipts);
    assert!(matches!(
        result.shards[spot as usize],
        ShardVerdict::Disputed { .. }
    ));
    assert!(crate::admission::undetected_fraud_bound(0.25, 10) < 0.06);
}

#[test]
fn replicas_prefer_different_backends() {
    let ids: Vec<NodeIdentity> = (1..=4).map(identity).collect();
    let mut pool = ComputePool::new(network()).with_min_age(0);
    // Two fast CUDA boxes, one ROCm box, one CPU-only box.
    let gpus = [
        vec![Device::parse_gpu("cuda:24576:5000").unwrap()],
        vec![Device::parse_gpu("cuda:24576:5000").unwrap()],
        vec![Device::parse_gpu("rocm:24576:2500").unwrap()],
        vec![],
    ];
    for (id, gpu) in ids.iter().zip(gpus) {
        pool.upsert(advert(id, 1, gpu, FormFactor::Server), None, false, NOW)
            .unwrap();
    }
    let mut job = spec(60, 2, DevicePreference::GpuOnly);
    let diverse = pool.plan(job.clone(), ids[0].node_id(), &ALL, NOW).unwrap();
    job.diverse_replicas = false;
    let plain = pool.plan(job, ids[0].node_id(), &ALL, NOW).unwrap();
    assert!(
        diverse.mean_backend_diversity() > plain.mean_backend_diversity(),
        "{} vs {}",
        diverse.mean_backend_diversity(),
        plain.mean_backend_diversity()
    );
}

#[test]
fn locality_doctrine_restricts_tight_and_pipeline_work() {
    let (pool, ids) = pool();
    let me = ids[1].node_id();
    let mut job = spec(10, 1, DevicePreference::Any);
    job.sync_class = SyncClass::Tight;
    let tight = pool.plan(job.clone(), me, &ALL, NOW).unwrap();
    assert!(tight.assignments.iter().all(|a| a.worker == me));
    job.sync_class = SyncClass::Pipeline;
    let near = ids[2].node_id();
    let rtt = move |n: &NodeId| (*n == near).then_some(12.0);
    let context = PlanContext {
        trust: &trust_all,
        rtt_ms: &rtt,
    };
    let pipeline = pool.plan(job, me, &context, NOW).unwrap();
    assert!(
        pipeline
            .assignments
            .iter()
            .all(|a| a.worker == me || a.worker == near)
    );
    assert!(pipeline.assignments.iter().any(|a| a.worker == near));
}

#[test]
fn new_nodes_ramp_up_before_they_count() {
    let ids: Vec<NodeIdentity> = (1..=2).map(identity).collect();
    let mut pool = ComputePool::new(network()).with_local(ids[0].node_id());
    for id in &ids {
        pool.upsert(advert(id, 1, vec![], FormFactor::Server), None, false, NOW)
            .unwrap();
    }
    let job = spec(20, 1, DevicePreference::Any);
    let fresh = pool.plan(job.clone(), ids[0].node_id(), &ALL, NOW).unwrap();
    assert!(
        fresh
            .assignments
            .iter()
            .all(|a| a.worker == ids[0].node_id()),
        "a just-seen peer has no weight yet; the local node is exempt"
    );
    // Fresh adverts later keep each node's first-seen time.
    let later = NOW + DEFAULT_MIN_AGE_MS / 2;
    for id in &ids {
        let mut a = advert(id, 2, vec![], FormFactor::Server).advert;
        a.issued_ms = later;
        pool.upsert(SignedAdvert::sign(id, a).unwrap(), None, false, later)
            .unwrap();
    }
    let half = pool.plan(job, ids[0].node_id(), &ALL, later).unwrap();
    assert!(
        half.assignments
            .iter()
            .any(|a| a.worker == ids[1].node_id())
    );
}
