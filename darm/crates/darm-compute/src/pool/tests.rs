use super::*;
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
        nonce: 7,
    }
}

/// Two CPU-only servers and one server with a GPU 10x a CPU.
fn pool() -> (ComputePool, Vec<NodeIdentity>) {
    let ids: Vec<NodeIdentity> = (1..=3).map(identity).collect();
    let mut pool = ComputePool::new(network());
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
            &trust_all,
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
            &trust_all,
            NOW,
        )
        .unwrap();
    assert_eq!(cpu_only.gpu_shards, 0);
    let gpu_only = pool
        .plan(
            spec(30, 1, DevicePreference::GpuOnly),
            ids[0].node_id(),
            &trust_all,
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
            &trust_all,
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
            &trust_all,
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
    let plan = pool.plan(job, ids[0].node_id(), &only_first, NOW).unwrap();
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
    let mut pool = ComputePool::with_capacity(network(), 2);
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
    let mut pool = ComputePool::new(network());
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
            &trust_all,
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
            &trust_all,
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
