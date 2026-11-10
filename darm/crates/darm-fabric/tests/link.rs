//! End-to-end behaviour of fabric links: real handshakes, real sealing.

use darm_core::{DarmError, Hash32, NodeIdentity};
use darm_fabric::channel::{ChannelId, ChannelSpec, ChannelTable, SyncClass};
use darm_fabric::link::{Inbound, Link};
use darm_fabric::scheduler::QueueLimits;
use darm_fabric::tensor::{Reassembler, ReassemblyLimits, Tensor, send_tensor};
use darm_fabric::transport::{MemoryTransport, TcpTransport, Transport};
use darm_fabric::workload::{Dispatcher, Endpoint, Outbound, Workload};
use darm_fabric::{envelope, establish};
use darm_protocol::{HandshakeConfig, Offer, Session, SessionLimits, StaticKey};
use std::net::{TcpListener, TcpStream};
use std::thread;
use std::time::Duration;

const NOW: u64 = 1_700_000_000_000;
const WAIT: Duration = Duration::from_secs(5);
const APP: ChannelId = ChannelId(0x8000);

struct Node {
    identity: NodeIdentity,
    key: StaticKey,
    network: Hash32,
}

impl Node {
    fn new(seed: u8, network: &[u8]) -> Self {
        Self {
            identity: NodeIdentity::from_secret_bytes(&[seed; 32]),
            key: StaticKey::generate().unwrap(),
            network: Hash32::digest("net", network),
        }
    }

    fn config(&self) -> HandshakeConfig<'_> {
        HandshakeConfig {
            identity: &self.identity,
            static_key: &self.key,
            network: self.network,
            offer: Offer::default(),
            limits: SessionLimits::default(),
        }
    }
}

fn channels() -> ChannelTable {
    let mut table = ChannelTable::standard();
    table
        .register(ChannelSpec {
            id: APP,
            name: "app",
            class: SyncClass::Pipeline,
            max_payload: 1024,
            burst: 3,
            per_second: 1,
            ttl_ms: None,
        })
        .unwrap();
    table
}

/// Handshake two nodes over an in-process carrier.
fn sessions(a: &Node, b: &Node) -> darm_core::Result<(Session, Session)> {
    sessions_within(a, b, WAIT)
}

fn sessions_within(a: &Node, b: &Node, wait: Duration) -> darm_core::Result<(Session, Session)> {
    let (mut ta, mut tb) = MemoryTransport::pair(16);
    thread::scope(|scope| {
        let responder = scope.spawn(move || establish::accept(&mut tb, &b.config(), wait));
        let initiator = establish::connect(&mut ta, &a.config(), wait);
        let responder = responder.join().expect("responder thread");
        Ok((initiator?, responder?))
    })
}

fn links() -> (Link, Link, Node, Node) {
    let (a, b) = (Node::new(1, b"t"), Node::new(2, b"t"));
    let (sa, sb) = sessions(&a, &b).unwrap();
    (
        Link::new(sa, channels(), QueueLimits::default()),
        Link::new(sb, channels(), QueueLimits::default()),
        a,
        b,
    )
}

/// Move everything queued on `from` into `to`, returning what was delivered.
fn deliver(from: &mut Link, to: &mut Link, now_ms: u64) -> Vec<darm_core::Result<Inbound>> {
    let mut out = Vec::new();
    while let Some(frames) = from.poll_outbound(now_ms).unwrap() {
        for frame in frames {
            match to.on_frame(&frame, now_ms) {
                Ok(Some(message)) => out.push(Ok(message)),
                Ok(None) => {}
                Err(error) => out.push(Err(error)),
            }
        }
    }
    out
}

#[test]
fn messages_arrive_attributed_to_the_proven_peer() {
    let (mut la, mut lb, a, b) = links();
    assert_eq!(la.peer(), b.identity.node_id());
    assert_eq!(lb.peer(), a.identity.node_id());

    assert_eq!(
        la.send(ChannelId::LEDGER, 9, true, b"vote".to_vec(), NOW)
            .unwrap(),
        0
    );
    assert_eq!(
        la.send(ChannelId::LEDGER, 9, false, b"more".to_vec(), NOW)
            .unwrap(),
        1
    );
    let got: Vec<Inbound> = deliver(&mut la, &mut lb, NOW)
        .into_iter()
        .map(Result::unwrap)
        .collect();
    assert_eq!(got.len(), 2);
    assert_eq!(got[0].peer, a.identity.node_id());
    assert_eq!(
        (got[0].channel, got[0].stream, got[0].seq, got[0].fin),
        (ChannelId::LEDGER, 9, 0, true)
    );
    assert_eq!(got[0].payload, b"vote");
    assert_eq!((got[1].seq, got[1].fin), (1, false));
}

#[test]
fn nodes_on_different_networks_cannot_link() {
    let (a, b) = (Node::new(1, b"alpha"), Node::new(2, b"beta"));
    assert!(sessions_within(&a, &b, Duration::from_millis(200)).is_err());
}

#[test]
fn tight_traffic_overtakes_a_bulk_backlog() {
    let (mut la, mut lb, ..) = links();
    for i in 0..200u32 {
        la.send(ChannelId::GOSSIP, i, false, vec![0; 4096], NOW)
            .unwrap();
    }
    la.send(ChannelId::LEDGER, 0, false, b"block".to_vec(), NOW)
        .unwrap();
    la.send(ChannelId::TENSOR, 0, false, b"activation".to_vec(), NOW)
        .unwrap();
    let order: Vec<ChannelId> = deliver(&mut la, &mut lb, NOW)
        .into_iter()
        .map(|m| m.unwrap().channel)
        .collect();
    assert_eq!(order.len(), 202);
    assert_eq!(order[0], ChannelId::TENSOR, "tight leaves first");
    assert_eq!(order[1], ChannelId::LEDGER, "block outranks async");
}

#[test]
fn stale_tight_messages_are_dropped_not_delivered_late() {
    let (mut la, mut lb, ..) = links();
    la.send(ChannelId::TENSOR, 0, false, b"old".to_vec(), NOW)
        .unwrap();
    // The standard tensor channel has a 250 ms deadline.
    assert!(deliver(&mut la, &mut lb, NOW + 250).is_empty());
    assert_eq!(la.scheduler().stats.expired, 1);
}

#[test]
fn policy_is_enforced_on_send() {
    let (mut la, ..) = links();
    assert!(matches!(
        la.send(ChannelId(0x9999), 0, false, Vec::new(), NOW),
        Err(DarmError::Rejected(_))
    ));
    assert!(matches!(
        la.send(APP, 0, false, vec![0; 1025], NOW),
        Err(DarmError::LimitExceeded(_))
    ));
    // A refused message must not consume a sequence number.
    assert_eq!(la.send(APP, 0, false, vec![0; 1024], NOW).unwrap(), 0);
}

#[test]
fn receiver_enforces_its_own_table_and_limits() {
    let (a, b) = (Node::new(1, b"t"), Node::new(2, b"t"));
    let (sa, sb) = sessions(&a, &b).unwrap();
    // The sender mounts a wider channel than the receiver allows, and a
    // channel the receiver does not have at all.
    let mut wide = ChannelTable::standard();
    wide.register(ChannelSpec {
        id: APP,
        name: "app",
        class: SyncClass::Pipeline,
        max_payload: 4096,
        burst: 100,
        per_second: 100,
        ttl_ms: None,
    })
    .unwrap();
    wide.register(ChannelSpec {
        id: ChannelId(0x8001),
        name: "extra",
        class: SyncClass::Async,
        max_payload: 64,
        burst: 100,
        per_second: 100,
        ttl_ms: None,
    })
    .unwrap();
    let mut la = Link::new(sa, wide, QueueLimits::default());
    let mut lb = Link::new(sb, channels(), QueueLimits::default());

    la.send(ChannelId(0x8001), 0, false, b"x".to_vec(), NOW)
        .unwrap();
    la.send(APP, 0, false, vec![0; 2048], NOW).unwrap();
    let results = deliver(&mut la, &mut lb, NOW);
    assert!(results.iter().all(Result::is_err));
    assert_eq!(lb.stats.unknown_channel, 1);
    assert_eq!(lb.stats.oversized, 1);

    // Burst of 3, refill 1/s on the receiver's table.
    for _ in 0..5 {
        la.send(APP, 0, false, b"ok".to_vec(), NOW).unwrap();
    }
    let results = deliver(&mut la, &mut lb, NOW);
    assert_eq!(results.iter().filter(|r| r.is_ok()).count(), 3);
    assert_eq!(lb.stats.rate_limited, 2);
    la.send(APP, 0, false, b"ok".to_vec(), NOW).unwrap();
    assert!(
        deliver(&mut la, &mut lb, NOW + 1_000)[0].is_ok(),
        "bucket refills"
    );
}

#[test]
fn forged_replayed_and_cross_link_frames_are_rejected() {
    let (mut la, mut lb, ..) = links();
    la.send(ChannelId::LEDGER, 0, false, b"vote".to_vec(), NOW)
        .unwrap();
    let frame = la.poll_outbound(NOW).unwrap().unwrap().remove(0);

    let mut forged = frame.clone();
    let last = forged.len() - 1;
    forged[last] ^= 1;
    assert!(lb.on_frame(&forged, NOW).is_err());
    assert!(lb.on_frame(&frame, NOW).unwrap().is_some());
    assert!(lb.on_frame(&frame, NOW).is_err(), "replay");
    assert!(lb.on_frame(&[0; 8], NOW).is_err(), "runt");

    // A frame sealed for one link means nothing on another.
    let (mut lc, mut ld, ..) = links();
    lc.send(ChannelId::LEDGER, 0, false, b"vote".to_vec(), NOW)
        .unwrap();
    let other = lc.poll_outbound(NOW).unwrap().unwrap().remove(0);
    assert!(lb.on_frame(&other, NOW).is_err());
    assert!(ld.on_frame(&other, NOW).unwrap().is_some());
    assert_eq!(lb.stats.delivered, 1);
    assert_eq!(lb.stats.rejected_frames, 4);
}

#[test]
fn full_queue_pushes_back() {
    let (a, b) = (Node::new(1, b"t"), Node::new(2, b"t"));
    let (sa, _sb) = sessions(&a, &b).unwrap();
    let mut limits = QueueLimits::default();
    limits.max_bytes[SyncClass::Tight.index()] = 100 + envelope::HEADER_LEN;
    let mut la = Link::new(sa, channels(), limits);
    la.send(ChannelId::TENSOR, 0, false, vec![0; 100], NOW)
        .unwrap();
    assert!(matches!(
        la.send(ChannelId::TENSOR, 0, false, vec![0; 1], NOW),
        Err(DarmError::LimitExceeded(_))
    ));
    // Other classes are unaffected.
    la.send(ChannelId::GOSSIP, 0, false, vec![0; 1], NOW)
        .unwrap();
}

#[test]
fn tensors_cross_a_link_interleaved_with_other_traffic() {
    let (mut la, mut lb, ..) = links();
    let values: Vec<f32> = (0..100_000).map(|i| i as f32 * 0.5).collect();
    let tensor = Tensor::from_f32(&[100, 1000], &values).unwrap();
    send_tensor(&mut la, ChannelId::TENSOR, 42, &tensor, NOW).unwrap();
    la.send(ChannelId::LEDGER, 1, false, b"vote".to_vec(), NOW)
        .unwrap();

    let mut reassembler = Reassembler::new(ReassemblyLimits::default());
    let mut received = None;
    let mut ledger = 0;
    for message in deliver(&mut la, &mut lb, NOW) {
        let message = message.unwrap();
        if message.channel == ChannelId::TENSOR {
            assert_eq!(message.stream, 42);
            if let Some(done) = reassembler
                .on_chunk(message.stream, &message.payload, NOW)
                .unwrap()
            {
                assert!(message.fin);
                received = Some(done);
            }
        } else {
            ledger += 1;
        }
    }
    assert_eq!(received.unwrap().to_f32(), values);
    assert_eq!(ledger, 1);

    // A tensor larger than the tight queue is refused whole.
    let huge = Tensor::from_f32(&[1_000_000], &vec![0.0; 1_000_000]).unwrap();
    assert!(send_tensor(&mut la, ChannelId::TENSOR, 43, &huge, NOW).is_err());
    assert!(la.scheduler().is_empty(), "nothing was partially queued");
}

struct Echo;

impl Workload for Echo {
    fn on_message(&mut self, message: &Inbound) -> darm_core::Result<Vec<Outbound>> {
        Ok(vec![Outbound {
            channel: message.channel,
            stream: message.stream,
            fin: true,
            payload: message.payload.iter().rev().copied().collect(),
        }])
    }
}

fn echo_round_trip<T: Transport + Send>(client: T, server: T) {
    let (a, b) = (Node::new(1, b"t"), Node::new(2, b"t"));
    thread::scope(|scope| {
        let b = &b;
        let serving = scope.spawn(move || {
            let mut endpoint = Endpoint::accept(
                server,
                &b.config(),
                channels(),
                QueueLimits::default(),
                WAIT,
            )
            .unwrap();
            let mut dispatcher = Dispatcher::default();
            dispatcher
                .mount(ChannelId::COMPUTE, Box::new(Echo))
                .unwrap();
            assert!(
                dispatcher
                    .mount(ChannelId::COMPUTE, Box::new(Echo))
                    .is_err()
            );
            assert!(endpoint.serve_once(&mut dispatcher, WAIT).unwrap());
            endpoint.link().peer()
        });
        let mut endpoint = Endpoint::connect(
            client,
            &a.config(),
            channels(),
            QueueLimits::default(),
            WAIT,
        )
        .unwrap();
        endpoint
            .send(ChannelId::COMPUTE, 5, false, b"shard".to_vec())
            .unwrap();
        assert!(endpoint.flush().unwrap());
        let reply = endpoint.recv(WAIT).unwrap().unwrap();
        assert_eq!(reply.payload, b"drahs");
        assert_eq!((reply.stream, reply.fin), (5, true));
        assert_eq!(reply.peer, b.identity.node_id());
        assert_eq!(serving.join().unwrap(), a.identity.node_id());
    });
}

#[test]
fn workload_serves_over_memory_carrier() {
    let (client, server) = MemoryTransport::pair(16);
    echo_round_trip(client, server);
}

#[test]
fn workload_serves_over_tcp_carrier() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let client = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
    let (server, _) = listener.accept().unwrap();
    echo_round_trip(
        TcpTransport::new(client).unwrap(),
        TcpTransport::new(server).unwrap(),
    );
}

#[test]
fn endpoint_keeps_sealed_frames_when_the_carrier_pushes_back() {
    let (a, b) = (Node::new(1, b"t"), Node::new(2, b"t"));
    let (sa, sb) = sessions(&a, &b).unwrap();
    let (ta, mut tb) = MemoryTransport::pair(2);
    let mut endpoint = Endpoint::attach(ta, Link::new(sa, channels(), QueueLimits::default()));
    let mut lb = Link::new(sb, channels(), QueueLimits::default());
    for i in 0..6u32 {
        endpoint
            .send(ChannelId::LEDGER, i, false, vec![i as u8])
            .unwrap();
    }
    let mut streams = Vec::new();
    let mut rounds = 0;
    loop {
        let done = endpoint.flush().unwrap();
        while let Some(frame) = tb.recv(Duration::from_millis(1)).unwrap() {
            let now = darm_core::time::now_ms();
            streams.push(lb.on_frame(&frame, now).unwrap().unwrap().stream);
        }
        rounds += 1;
        if done {
            break;
        }
    }
    assert!(rounds > 1, "the carrier pushed back at least once");
    assert_eq!(streams, [0, 1, 2, 3, 4, 5], "nothing lost, order kept");
}

#[test]
fn flush_limit_bounds_work_and_sends_tight_first() {
    let (a, b) = (Node::new(1, b"t"), Node::new(2, b"t"));
    let (sa, sb) = sessions(&a, &b).unwrap();
    let (ta, mut tb) = MemoryTransport::pair(16);
    let mut endpoint = Endpoint::attach(ta, Link::new(sa, channels(), QueueLimits::default()));
    let mut lb = Link::new(sb, channels(), QueueLimits::default());
    endpoint
        .send(ChannelId::GOSSIP, 0, false, b"bulk".to_vec())
        .unwrap();
    endpoint
        .send(ChannelId::GOSSIP, 1, false, b"bulk".to_vec())
        .unwrap();
    endpoint
        .send(ChannelId::TENSOR, 2, false, b"now".to_vec())
        .unwrap();
    assert!(!endpoint.flush_limit(1).unwrap(), "two messages remain");
    let frame = tb.recv(Duration::from_millis(50)).unwrap().unwrap();
    let first = lb
        .on_frame(&frame, darm_core::time::now_ms())
        .unwrap()
        .unwrap();
    assert_eq!(first.channel, ChannelId::TENSOR);
    assert!(tb.recv(Duration::from_millis(1)).unwrap().is_none());
    assert!(endpoint.flush().unwrap());
}

#[test]
fn largest_single_frame_payload_seals_into_exactly_one_frame() {
    use darm_fabric::channel::MAX_SINGLE_FRAME_PAYLOAD;
    let (mut la, mut lb, ..) = links();
    la.send(
        ChannelId::TENSOR,
        0,
        false,
        vec![7; MAX_SINGLE_FRAME_PAYLOAD],
        NOW,
    )
    .unwrap();
    let frames = la.poll_outbound(NOW).unwrap().unwrap();
    assert_eq!(frames.len(), 1, "tight messages are never fragmented");
    assert!(frames[0].len() <= darm_fabric::transport::MAX_WIRE_FRAME);
    let got = lb.on_frame(&frames[0], NOW).unwrap().unwrap();
    assert_eq!(got.payload.len(), MAX_SINGLE_FRAME_PAYLOAD);

    // A full-size bulk message fragments and still arrives whole.
    use darm_fabric::channel::MAX_PAYLOAD;
    la.send(ChannelId::GOSSIP, 0, false, vec![9; MAX_PAYLOAD], NOW)
        .unwrap();
    let got = deliver(&mut la, &mut lb, NOW).remove(0).unwrap();
    assert_eq!(got.payload.len(), MAX_PAYLOAD);
}
