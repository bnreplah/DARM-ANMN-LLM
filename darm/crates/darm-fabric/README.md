# darm-fabric

The DARM-ANN fabric: a transport-agnostic shim that lets many workloads share
one authenticated link between two nodes.

[`darm-protocol`](../darm-protocol) gives two nodes a negotiated, identity-bound
session. This crate turns that session into a **link** with prioritized
channels, so memory gossip, ledger traffic, compute jobs and low-latency
tensor streams can run side by side without one delaying another, over any
carrier that can move opaque frames.

## What it owns

| Module | Responsibility |
|---|---|
| `channel` | Channel ids, the four sync classes (`async`, `block`, `pipeline`, `tight`, the protocol's section 8 codes) and the per-link channel table. Built-in channels: gossip, ledger, compute, tensor. Applications mount their own from id `0x8000`. |
| `envelope` | The 12-byte header (`version, channel, flags, stream, seq`) that multiplexes channels inside a sealed session message. |
| `scheduler` | Bounded outbound queues. `tight` is strict priority and expires instead of arriving late; `pipeline`, `block` and `async` share the link 4:2:1. A full queue refuses work. |
| `link` | One peer link as an I/O-free state machine: seal outbound, open inbound, attribute every message to the peer the handshake proved, refuse unknown channels, oversized payloads and floods. |
| `tensor` | Typed, shape-checked tensors (`f32`, or `i8` quantized at a quarter of the size), chunked to one frame each, reassembled in any order under hard memory bounds, verified by digest. |
| `transport` | The `Transport` trait (send a frame, receive a frame) with an in-process and a TCP carrier. |
| `establish` | Runs the `darm-protocol` handshake over a carrier, with a size cap and a deadline. |
| `workload` | The `Workload` trait, the `Dispatcher` that routes messages to mounted workloads, and `Endpoint`, which joins a link to a carrier. |

It does not own cryptography (that is `darm-protocol` and `darm-core`), any
particular workload, or discovery of peers.

## Using it

```rust,ignore
// One side accepts, the other connects; both get an Endpoint.
let mut endpoint = Endpoint::connect(
    TcpTransport::new(stream)?, &handshake_config,
    ChannelTable::standard(), QueueLimits::default(), timeout)?;

// The peer is proven by the handshake, not claimed by a header.
let peer = endpoint.link().peer();

// Send an activation on the tight tensor channel.
let hidden = Tensor::quantize_i8(&[1, 4096], &values)?;
send_tensor(endpoint.link_mut(), ChannelId::TENSOR, stream_id, &hidden, now_ms())?;
endpoint.flush()?;
```

A workload implements one method and is mounted on a channel:

```rust,ignore
impl Workload for MyExecutor {
    fn on_message(&mut self, message: &Inbound) -> Result<Vec<Outbound>> { /* ... */ }
}
dispatcher.mount(ChannelId::COMPUTE, Box::new(MyExecutor))?;
endpoint.serve_once(&mut dispatcher, timeout)?;
```

## Security model

- Every byte on a link is sealed by the session; the sender of a message is
  the node whose Ed25519 identity signed the handshake. Forged, replayed and
  cross-link frames fail authentication and are counted.
- A channel's class and limits come from the receiver's own table and never
  from the wire, so a peer cannot promote its traffic or raise its own caps.
- Everything decoded is bounded before memory is committed: frame length
  prefixes, envelope sizes, tensor shapes (checked multiplication), chunk
  counts, concurrent reassemblies, buffered bytes and reassembly time. A
  chunk announcing a 64 MiB tensor reserves a slot table, not 64 MiB.
- Inbound traffic is rate limited per channel; outbound queues are bounded
  per class, so a link's memory use has a ceiling in both directions.
- Unknown envelope versions and reserved flag bits are rejected.

Not covered: traffic analysis beyond the session's padding (channel timing is
visible to an observer), and peers that are authenticated but malicious at
the workload level. Workloads must still verify what they are told; DARM
records keep their own signatures for that reason.

## Measured latency

`cargo run --release -p darm-fabric --example fabric_latency` on a 2-core
cloud VM, TCP loopback, ChaCha20-Poly1305 with power-of-two padding, 2,000
round trips per row (send a 4096-value activation, receive an
acknowledgement), three runs:

| Case | p50 | p99 |
|---|---|---|
| `f32` (16 KiB), idle link | 0.20 ms | 0.29 to 0.31 ms |
| `i8` (4 KiB), idle link | 0.08 ms | 0.13 to 0.15 ms |
| `f32` on the tight channel, 512 KiB of bulk queued ahead | 0.21 ms | 0.36 to 0.60 ms |
| `f32` on an async channel, same 512 KiB queued ahead | 1.5 ms | 2.2 to 2.7 ms |

These bound the fabric's own cost (sealing, framing, scheduling). They are
loopback numbers and say nothing about a real network. Two limits are worth
knowing:

- Priority acts on what is still queued at the sender. Bytes already handed
  to an ordered carrier such as TCP cannot be overtaken (head-of-line
  blocking). A carrier with independent streams per class removes that.
- `Endpoint` is synchronous and serves one link. An async adapter and the
  mesh-llm channel carrier are the next step; `Link` needs no changes for
  either because it performs no I/O.

## Development

```bash
export RUSTC_WRAPPER=
cargo test -p darm-fabric
cargo clippy -p darm-fabric --all-targets -- -D warnings
```
