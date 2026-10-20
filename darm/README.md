# DARM-ANN inside mesh-llm

This directory is what turns [mesh-llm](../README.md) into **DARM-ANN-LLM**:
a mesh of LLM nodes that shares a signed, tamper-evident distributed memory,
routes requests like an ant colony, and governs its own growth through
validator votes.

DARM-ANN concepts come from the original proof of concept (kept verbatim in
[`legacy-poc/`](legacy-poc/)). Every concept was re-implemented in Rust and
attached to real mesh signals; the simulated parts of the PoC were replaced
(see [What changed from the PoC](#what-changed-from-the-poc)).

Server and infrastructure for running DARM-ANN outside a mesh node (archive
node, Ethereum anchor contract, Docker/Kubernetes deployment, the wire
protocol spec for other implementations) live in the companion repository
[`bnreplah/DARM-ANN`](https://github.com/bnreplah/DARM-ANN).

## Architecture

```text
 OpenAI client ──► mesh-llm ingress ──► virtual model "darm"  (darm-mesh-plugin)
                                          │ 1. recall: distributed memory search
                                          │ 2. route: pheromone-ranked candidates
                                          │ 3. learn: trail/danger + remember
                                          ▼
                         concrete models on this node or mesh peers (Skippy)

 darm-mesh-plugin ◄──── mesh channel "darm.v1" (iroh QUIC) ────► peers
   ├─ darm-memory   signed content-addressed memories, short/long-term tiers
   ├─ darm-core     identities, canonical encoding, Merkle, quorum ledger
   └─ darm-swarm    lifecycle, pheromones, genome, governed replication
```

| Crate | Role |
|---|---|
| [`darm-core`](crates/darm-core) | Ed25519 node identities; canonical, domain-separated signing encoding; BLAKE3 Merkle trees with inclusion proofs; the memory ledger (signed entries, blocks final at >2/3 validator weight, one vote per height). |
| [`darm-memory`](crates/darm-memory) | Distributed associative memory: signed records, `Local`/`Mesh` scopes, decay and consolidation, hashing embedder, author-only tombstones, anti-entropy inventory, injection-hardened prompt rendering. |
| [`darm-swarm`](crates/darm-swarm) | Biomimetic lifecycle (germinating → active → flowering → reproducing, resting, damaged/healing) driven by measured success, latency and load; pheromone field with half-life decay and ACO ranking; deterministic genomes; replication *recommendations*. |
| [`darm-mesh-plugin`](crates/darm-mesh-plugin) | The built-in mesh-llm plugin: `darm` virtual model, gossip on `darm.v1`, ledger block production, MCP/HTTP operations. Registered by `mesh-llm-host-runtime` as an optional in-process built-in. |

## Using it

Every `mesh-llm serve` node now advertises a `darm` model alongside its
concrete models:

```bash
curl http://localhost:9337/v1/chat/completions -H 'Content-Type: application/json' -d '{
  "model": "darm",
  "messages": [{"role": "user", "content": "What port does the staging gateway use?"}],
  "darm": {"namespace": "ops-team", "remember": true, "recall": true, "top_k": 5}
}'
```

The optional `darm` object is stripped before the request reaches a model.
Responses carry `x-darm-memories` (memories injected), `x-darm-route`
(concrete model), `x-darm-node` and, when the turn was remembered,
`x-darm-memory-id`.

Operations (projected by the host as MCP tools and plugin HTTP):
`darm_status`, `darm_memory_search`, `darm_memory_store`,
`darm_memory_forget`, `darm_ledger_proof`.

## Configuration

| Variable | Default | Meaning |
|---|---|---|
| `MESH_LLM_DARM` | on | `off` to not start the built-in at all. |
| `DARM_DATA_DIR` | `~/.mesh-llm/darm` | Identity key (0600), ledger and memory logs. Directory is set to 0700. |
| `DARM_CHAIN_NAME` | `darm-local-<node>` | Ledger chain name. All validators of one chain must agree. |
| `DARM_VALIDATORS` | this node | Comma-separated DARM node ids (hex Ed25519 keys). More than one enables ledger gossip. |
| `DARM_TRUSTED_AUTHORS` | none | Node ids whose shared memories may be injected into prompts (validators are trusted too). |
| `DARM_REMEMBER` | `1` | Remember chat exchanges (locally unless sharing is on). |
| `DARM_SHARE` | `0` | New memories are mesh-scoped, committed to the ledger, and gossiped. |
| `DARM_ALLOW_PUBLIC_GOSSIP` | `0` | Permit memory/ledger gossip on a public mesh. |
| `DARM_PROPOSE_REPLICATION` | `0` | Submit `replicate` ledger proposals when the swarm policy recommends it. |
| `DARM_RECALL_TOP_K` / `DARM_RECALL_BUDGET_CHARS` | 5 / 4000 | Memory injection size. |
| `DARM_CAPACITY` / `DARM_TARGET_LATENCY_MS` | 8 / 8000 | Lifecycle energy and health calibration. |

A three-node private chain: run each node once to create its identity
(`darm_status` shows `node_id`), then on every node set the same
`DARM_CHAIN_NAME`, the same `DARM_VALIDATORS=<id1>,<id2>,<id3>`, and
`DARM_SHARE=1`.

## Security model

DARM-ANN was built security-first. The assumptions and controls:

- **Peer frames are unauthenticated.** mesh-llm plugin channel frames do not
  sign their claimed `source_peer_id` yet. DARM therefore never trusts the
  sender: every memory, tombstone, ledger entry, block and attestation carries
  its own Ed25519 signature (`verify_strict`, no malleability) over a
  canonical, domain-separated encoding bound to the chain id. The claimed peer
  id is used only for replies and rate limiting.
- **Ledger safety.** Blocks finalize only with attestations from validators
  holding strictly more than 2/3 of the weight; each validator signs at most
  one block per height. Logs on disk are replayed and fully re-verified on
  load, so a tampered history fails to open. Reputation reports and votes are
  validator-only; no node can report on itself; only an author can revoke its
  memory.
- **Indirect prompt injection.** Shared memories are untrusted text placed in
  the system prompt. Only memories from this node, validators, or
  `DARM_TRUSTED_AUTHORS` are ever injected; injected text is fenced, stripped
  of control characters, and fence-forging tags are neutralized, with an
  explicit "data, not instructions" preamble. This reduces but cannot
  eliminate the risk; keep the trust set small.
- **Privacy.** Memories default to `Local` scope and never leave the node.
  Peers can only receive `Mesh`-scoped records, and gossip is off on public
  meshes unless explicitly enabled. Namespaces (`darm.namespace`) isolate
  recall between callers of one node; callers are not authenticated by the
  plugin, so on multi-tenant nodes set `DARM_REMEMBER=0` or put an
  authenticating proxy in front that assigns namespaces.
- **Resource exhaustion.** Channel bodies are capped at 1 MiB before parsing,
  per-message item counts are capped, inbound gossip is token-bucket rate
  limited per peer, pending ledger entries are capped globally and per author,
  candidate blocks are capped, and a single peer can occupy at most a bounded
  share of the memory store. Local and long-term memories are never evicted
  for gossip.
- **No autonomous self-replication.** The PoC spawned Kubernetes pods,
  Docker containers and MPI processes on its own. A self-replicating workload
  holding cluster credentials is indistinguishable from a worm, so here the
  swarm only *recommends* replication; a recommendation becomes a ledger
  proposal that validators vote on, and only the separately deployed,
  operator-controlled replicator in the DARM-ANN infrastructure repo may act
  on a passed proposal.
- **Keys at rest.** The identity key is created 0600 and refused if other
  users can read it; secrets are zeroized and never printed.

## What changed from the PoC

| PoC | DARM-ANN-LLM |
|---|---|
| XOR cipher "quantum" encryption, HMAC "signatures" verifiable only by the signer | Ed25519 signatures on every record; transport encryption is mesh-llm's QUIC/TLS. Signatures are algorithm-tagged so ML-DSA (FIPS 204) can be added as a hybrid. |
| Proof-of-work "Proof of Learning" mining | Validator-attested finality (no wasted compute, Sybil-resistant in an admitted mesh). |
| In-process queue pretending to be MPI | Real peer-to-peer gossip on mesh-llm's iroh transport. |
| Simulated health/energy | Health from request success and latency, energy from in-flight load. |
| Ethereum stores memory content | Only hashes and checkpoints are anchored (see DARM-ANN `contracts/`); content stays off-chain. |
| Auto-spawning offspring | Deterministic genomes + replication proposals gated by a validator vote. |

## Development

```bash
export RUSTC_WRAPPER=   # upstream config wraps rustc with sccache
cargo test -p darm-core -p darm-memory -p darm-swarm -p darm-mesh-plugin
cargo clippy -p darm-core -p darm-memory -p darm-swarm -p darm-mesh-plugin --all-targets -- -D warnings
```

See [`UPSTREAM.md`](UPSTREAM.md) for how the mesh-llm sources were imported
and how to sync them, and [`docs/ROADMAP.md`](docs/ROADMAP.md) for what is
next.
