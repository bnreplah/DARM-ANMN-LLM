use super::*;
use crate::record::MemoryKind;

const NOW: u64 = 1_700_000_000_000;

fn identity(seed: u8) -> NodeIdentity {
    NodeIdentity::from_secret_bytes(&[seed; 32])
}

fn record(content: &str, scope: Scope, importance_milli: u16) -> MemoryRecord {
    MemoryRecord {
        author: identity(0).node_id(),
        created_ms: NOW,
        kind: MemoryKind::Semantic,
        scope,
        importance_milli,
        valence_milli: 0,
        tags: vec!["test".into()],
        content: content.into(),
        parent: None,
    }
}

#[test]
fn remember_and_recall() {
    let me = identity(1);
    let mut store = MemoryStore::new(MemoryLimits::default());
    store
        .remember(
            &me,
            record(
                "The staging cluster uses port 9337 for the OpenAI API",
                Scope::Local,
                500,
            ),
        )
        .unwrap();
    store
        .remember(
            &me,
            record("Sourdough needs a long cold proof", Scope::Local, 500),
        )
        .unwrap();
    let hits = store.search(
        "which port does the openai api use",
        &SearchOptions::default(),
        NOW,
    );
    assert_eq!(hits.len(), 1);
    assert!(hits[0].record.content.contains("9337"));
}

#[test]
fn peers_cannot_inject_forged_or_local_records() {
    let alice = identity(1);
    let mut store = MemoryStore::new(MemoryLimits::default());

    let mut forged = SignedMemory::sign(&alice, record("x is y", Scope::Mesh, 1)).unwrap();
    forged.record.content = "x is z".into();
    assert_eq!(
        store.insert(forged, Origin::Peer, NOW),
        Err(DarmError::InvalidSignature)
    );

    let private = SignedMemory::sign(&alice, record("secret", Scope::Local, 1)).unwrap();
    assert!(matches!(
        store.insert(private, Origin::Peer, NOW),
        Err(DarmError::Rejected(_))
    ));

    let mut future = record("from the future", Scope::Mesh, 1);
    future.created_ms = NOW + 3_600_000;
    let future = SignedMemory::sign(&alice, future).unwrap();
    assert!(store.insert(future, Origin::Peer, NOW).is_err());
}

#[test]
fn tombstones_are_author_only_and_block_resurrection() {
    let alice = identity(1);
    let bob = identity(2);
    let mut store = MemoryStore::new(MemoryLimits::default());
    let signed = store
        .remember(&alice, record("alpha fact", Scope::Mesh, 1))
        .unwrap();
    let id = signed.id();
    assert!(matches!(
        store.forget(&bob, &id, NOW),
        Err(DarmError::Unauthorized(_))
    ));
    let forged = SignedTombstone::sign(&bob, id, NOW);
    assert!(matches!(
        store.apply_tombstone(forged),
        Err(DarmError::Unauthorized(_))
    ));
    let tombstone = store.forget(&alice, &id, NOW).unwrap();
    assert!(store.get(&id).is_none());
    assert!(
        store.insert(signed.clone(), Origin::Peer, NOW).is_err(),
        "no resurrection"
    );

    let mut peer = MemoryStore::new(MemoryLimits::default());
    peer.insert(signed, Origin::Peer, NOW).unwrap();
    assert!(peer.apply_tombstone(tombstone).unwrap());
    assert!(peer.get(&id).is_none());
}

#[test]
fn per_peer_quota_and_eviction_protect_local_memories() {
    let me = identity(1);
    let flooder = identity(2);
    let limits = MemoryLimits {
        max_records: 3,
        max_records_per_peer: 2,
        ..MemoryLimits::default()
    };
    let mut store = MemoryStore::new(limits);
    store
        .remember(&me, record("my own note", Scope::Local, 100))
        .unwrap();
    for i in 0..2 {
        let m = SignedMemory::sign(&flooder, record(&format!("spam {i}"), Scope::Mesh, 0)).unwrap();
        store.insert(m, Origin::Peer, NOW).unwrap();
    }
    let third = SignedMemory::sign(&flooder, record("spam 2", Scope::Mesh, 0)).unwrap();
    assert!(matches!(
        store.insert(third, Origin::Peer, NOW),
        Err(DarmError::LimitExceeded(_))
    ));
    // A different peer's record evicts a weak peer record, never the local one.
    let other = SignedMemory::sign(&identity(3), record("useful", Scope::Mesh, 900)).unwrap();
    store.insert(other, Origin::Peer, NOW).unwrap();
    assert_eq!(store.len(), 3);
    assert_eq!(store.stats().local, 1);
}

#[test]
fn consolidation_by_recall_and_finality() {
    let me = identity(1);
    let mut store = MemoryStore::new(MemoryLimits::default());
    let a = store
        .remember(
            &me,
            record("kubernetes namespaces isolate workloads", Scope::Local, 100),
        )
        .unwrap()
        .id();
    assert_eq!(store.tier(&a), Some(Tier::ShortTerm));
    for _ in 0..CONSOLIDATION_RECALLS {
        store.search("kubernetes namespaces", &SearchOptions::default(), NOW);
    }
    assert_eq!(store.tier(&a), Some(Tier::LongTerm));

    let b = store
        .remember(
            &me,
            record("ledger finality needs two thirds", Scope::Mesh, 100),
        )
        .unwrap()
        .id();
    store.mark_finalized(&b).unwrap();
    assert_eq!(store.tier(&b), Some(Tier::LongTerm));
    let important = store
        .remember(
            &me,
            record("critical: never expose the admin port", Scope::Local, 950),
        )
        .unwrap()
        .id();
    assert_eq!(store.tier(&important), Some(Tier::LongTerm));
}

#[test]
fn trust_and_finality_filters() {
    let me = identity(1);
    let stranger = identity(9);
    let mut store = MemoryStore::new(MemoryLimits::default());
    let m = SignedMemory::sign(
        &stranger,
        record("deploy with --insecure flag", Scope::Mesh, 900),
    )
    .unwrap();
    let id = m.id();
    store.insert(m, Origin::Peer, NOW).unwrap();
    let me_id = me.node_id();
    let only_me = move |author: &NodeId| if *author == me_id { 1.0 } else { 0.0 };
    let options = SearchOptions {
        trust: &only_me,
        ..SearchOptions::default()
    };
    assert!(store.search("deploy flag", &options, NOW).is_empty());

    let finalized_only = SearchOptions {
        require_finalized_from_peers: true,
        ..SearchOptions::default()
    };
    assert!(store.search("deploy flag", &finalized_only, NOW).is_empty());
    store.mark_finalized(&id).unwrap();
    assert_eq!(store.search("deploy flag", &finalized_only, NOW).len(), 1);
}

#[test]
fn inventory_and_export_never_leak_local_scope() {
    let me = identity(1);
    let mut store = MemoryStore::new(MemoryLimits::default());
    let local = store
        .remember(&me, record("private", Scope::Local, 1))
        .unwrap()
        .id();
    let shared = store
        .remember(&me, record("public", Scope::Mesh, 1))
        .unwrap()
        .id();
    assert_eq!(store.inventory(10), vec![shared]);
    assert_eq!(store.export_shareable(&[local, shared]).len(), 1);
    assert_eq!(
        store.missing(&[local, shared, Hash32::ZERO, Hash32::ZERO]),
        vec![Hash32::ZERO]
    );
}

#[test]
fn persistent_log_round_trip_and_tamper_detection() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("memory.jsonl");
    let me = identity(1);
    let id;
    {
        let mut store = MemoryStore::open(MemoryLog::new(&path), MemoryLimits::default()).unwrap();
        id = store
            .remember(&me, record("persist me", Scope::Mesh, 300))
            .unwrap()
            .id();
        let gone = store
            .remember(&me, record("forget me", Scope::Mesh, 300))
            .unwrap()
            .id();
        store.forget(&me, &gone, NOW).unwrap();
        store.mark_finalized(&id).unwrap();
    }
    let store = MemoryStore::open(MemoryLog::new(&path), MemoryLimits::default()).unwrap();
    assert_eq!(store.len(), 1);
    assert_eq!(store.tier(&id), Some(Tier::LongTerm));

    let text = std::fs::read_to_string(&path).unwrap();
    std::fs::write(&path, text.replace("persist me", "persist ME")).unwrap();
    assert!(MemoryStore::open(MemoryLog::new(&path), MemoryLimits::default()).is_err());
}

#[test]
fn namespace_tag_isolates_recall() {
    let me = identity(1);
    let mut store = MemoryStore::new(MemoryLimits::default());
    let mut a = record("alice prefers dark mode in the console", Scope::Local, 500);
    a.tags = vec!["ns:alice".into()];
    store.remember(&me, a).unwrap();
    let bob_view = SearchOptions {
        required_tag: Some("ns:bob"),
        ..SearchOptions::default()
    };
    assert!(store.search("console dark mode", &bob_view, NOW).is_empty());
    let alice_view = SearchOptions {
        required_tag: Some("ns:alice"),
        ..SearchOptions::default()
    };
    assert_eq!(store.search("console dark mode", &alice_view, NOW).len(), 1);
}
