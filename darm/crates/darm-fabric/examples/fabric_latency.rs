//! Measures round-trip latency of activation tensors over a sealed fabric
//! link on TCP loopback, with and without a bulk backlog on the same link.
//!
//! ```text
//! cargo run --release -p darm-fabric --example fabric_latency
//! ```
//!
//! Each round the client queues `BACKLOG` bulk messages and one 4096-value
//! activation tensor, sends, and waits for the server's acknowledgement.
//! The tensor travels either on the `tight` tensor channel (leaves first) or
//! on an `async` channel (waits its turn behind the backlog), which isolates
//! what the scheduler buys. Between rounds the client waits for the server
//! to catch up, so only the sender-side queue is being measured.
//!
//! Loopback numbers bound the fabric's own overhead (sealing, framing,
//! scheduling); they say nothing about a real network. Priority acts on what
//! is still queued: bytes already handed to an ordered carrier such as TCP
//! cannot be overtaken.

use darm_core::time::now_ms;
use darm_core::{Hash32, NodeIdentity, Result};
use darm_fabric::channel::{
    ChannelId, ChannelSpec, ChannelTable, MAX_SINGLE_FRAME_PAYLOAD, SyncClass,
};
use darm_fabric::scheduler::QueueLimits;
use darm_fabric::tensor::{Reassembler, ReassemblyLimits, Tensor, send_tensor};
use darm_fabric::transport::TcpTransport;
use darm_fabric::workload::Endpoint;
use darm_protocol::{HandshakeConfig, Offer, SessionLimits, StaticKey};
use std::net::{TcpListener, TcpStream};
use std::thread;
use std::time::{Duration, Instant};

const BULK: ChannelId = ChannelId(0x8000);
const SLOW_TENSOR: ChannelId = ChannelId(0x8001);
const ACK: ChannelId = ChannelId(0x8002);
/// Sent after each round's leftovers; its echo proves the server has caught
/// up, so one round's backlog never leaks into the next round's timing.
const DRAIN: ChannelId = ChannelId(0x8003);
const ROUNDS: usize = 2_000;
const BACKLOG: usize = 16;
const WAIT: Duration = Duration::from_secs(10);

fn channels() -> Result<ChannelTable> {
    let mut table = ChannelTable::standard();
    let app = |id, name, class| ChannelSpec {
        id,
        name,
        class,
        max_payload: MAX_SINGLE_FRAME_PAYLOAD,
        burst: 1_000_000,
        per_second: 1_000_000,
        ttl_ms: None,
    };
    table.register(app(BULK, "bulk", SyncClass::Async))?;
    table.register(app(SLOW_TENSOR, "slow-tensor", SyncClass::Async))?;
    table.register(app(ACK, "ack", SyncClass::Tight))?;
    table.register(app(DRAIN, "drain", SyncClass::Async))?;
    Ok(table)
}

struct Node {
    identity: NodeIdentity,
    key: StaticKey,
}

impl Node {
    fn new() -> Result<Self> {
        Ok(Self {
            identity: NodeIdentity::generate()?,
            key: StaticKey::generate()?,
        })
    }

    fn config(&self) -> HandshakeConfig<'_> {
        HandshakeConfig {
            identity: &self.identity,
            static_key: &self.key,
            network: Hash32::digest("network", b"fabric-latency"),
            offer: Offer::default(),
            limits: SessionLimits::default(),
        }
    }
}

/// Acknowledge every completed tensor; discard bulk.
fn serve(stream: TcpStream) -> Result<()> {
    let node = Node::new()?;
    let mut endpoint = Endpoint::accept(
        TcpTransport::new(stream)?,
        &node.config(),
        channels()?,
        QueueLimits::default(),
        WAIT,
    )?;
    let mut reassembler = Reassembler::new(ReassemblyLimits::default());
    while let Ok(Some(message)) = endpoint.recv(WAIT) {
        if message.channel == BULK {
            continue;
        }
        if message.channel == DRAIN {
            endpoint.send(DRAIN, message.stream, true, Vec::new())?;
            endpoint.flush()?;
            continue;
        }
        if reassembler
            .on_chunk(message.stream, &message.payload, now_ms())?
            .is_some()
        {
            endpoint.send(ACK, message.stream, true, Vec::new())?;
            endpoint.flush()?;
        }
    }
    Ok(())
}

fn percentile(sorted: &[Duration], p: f64) -> Duration {
    sorted[((sorted.len() - 1) as f64 * p).round() as usize]
}

fn run(
    endpoint: &mut Endpoint<TcpTransport>,
    label: &str,
    channel: ChannelId,
    tensor: &Tensor,
    backlog: usize,
) -> Result<()> {
    let mut samples = Vec::with_capacity(ROUNDS);
    for round in 0..ROUNDS as u32 {
        for _ in 0..backlog {
            endpoint.send(BULK, round, false, vec![0; 32 * 1024])?;
        }
        let start = Instant::now();
        send_tensor(endpoint.link_mut(), channel, round, tensor, now_ms())?;
        // One frame per turn, as an event loop would, checking for the
        // acknowledgement between frames.
        let mut ack = None;
        while ack.is_none() {
            let drained = endpoint.flush_limit(1)?;
            let wait = if drained { WAIT } else { Duration::ZERO };
            ack = endpoint.recv(wait)?;
        }
        samples.push(start.elapsed());
        endpoint.send(DRAIN, round, true, Vec::new())?;
        endpoint.flush()?;
        while endpoint.recv(WAIT)?.is_some_and(|m| m.channel != DRAIN) {}
    }
    samples.sort();
    println!(
        "{label:<44} p50 {:>7.1} us   p99 {:>7.1} us   max {:>7.1} us",
        percentile(&samples, 0.50).as_secs_f64() * 1e6,
        percentile(&samples, 0.99).as_secs_f64() * 1e6,
        percentile(&samples, 1.0).as_secs_f64() * 1e6,
    );
    Ok(())
}

fn main() -> Result<()> {
    let listener = TcpListener::bind("127.0.0.1:0")?;
    let addr = listener.local_addr()?;
    let server = thread::spawn(move || -> Result<()> {
        let (stream, _) = listener.accept()?;
        serve(stream)
    });

    let node = Node::new()?;
    let mut endpoint = Endpoint::connect(
        TcpTransport::new(TcpStream::connect(addr)?)?,
        &node.config(),
        channels()?,
        QueueLimits::default(),
        WAIT,
    )?;
    let selected = endpoint.link().session().selected();
    println!("session: {selected:?}");
    println!("{ROUNDS} round trips each, 4096-value activation, TCP loopback\n");

    let values: Vec<f32> = (0..4096).map(|i| (i as f32 * 0.01).sin()).collect();
    let full = Tensor::from_f32(&[1, 4096], &values)?;
    let small = Tensor::quantize_i8(&[1, 4096], &values)?;

    run(
        &mut endpoint,
        "f32 (16 KiB), idle link",
        ChannelId::TENSOR,
        &full,
        0,
    )?;
    run(
        &mut endpoint,
        "i8  (4 KiB), idle link",
        ChannelId::TENSOR,
        &small,
        0,
    )?;
    let busy = format!("f32, tight, behind {} KiB backlog", BACKLOG * 32);
    run(&mut endpoint, &busy, ChannelId::TENSOR, &full, BACKLOG)?;
    let slow = format!("f32, async, behind {} KiB backlog", BACKLOG * 32);
    run(&mut endpoint, &slow, SLOW_TENSOR, &full, BACKLOG)?;

    drop(endpoint);
    server.join().expect("server thread")?;
    Ok(())
}
