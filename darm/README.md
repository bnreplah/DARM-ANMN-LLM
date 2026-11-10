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
   │                  └─ optional Noise sessions: sealed, padded, morphing frames
   ├─ darm-memory   signed content-addressed memories, short/long-term tiers
   ├─ darm-core     identities, canonical encoding, Merkle, quorum ledger
   ├─ darm-swarm    lifecycle, pheromones, genome, governed replication
   ├─ darm-compute  CPU/GPU/NPU/phone pool, heterogeneous scheduler, receipts
   ├─ darm-protocol polymorphic negotiation, identity-bound sessions, adaptation
   └─ darm-fabric   prioritized channels over a session on any carrier (not yet
                    wired into the plugin; see the crate README)
```

| Crate | Role |
|---|---|
| [`darm-core`](crates/darm-core) | Ed25519 node identities; canonical, domain-separated signing encoding; BLAKE3 Merkle trees with inclusion proofs; the memory ledger (signed entries, blocks final at >2/3 validator weight, one vote per height); and lock-based BFT rounds (`ledger::locking`), an opt-in replacement for the one-vote rule (`DARM_CONSENSUS=locking`). |
| [`darm-memory`](crates/darm-memory) | Distributed associative memory: signed records, `Local`/`Mesh` scopes, decay and consolidation, hashing embedder, author-only tombstones, anti-entropy inventory, injection-hardened prompt rendering. |
| [`darm-swarm`](crates/darm-swarm) | Biomimetic lifecycle (germinating → active → flowering → reproducing, resting, damaged/healing) driven by measured success, latency and load; pheromone field with half-life decay and ACO ranking; deterministic genomes; replication *recommendations*. |
| [`darm-compute`](crates/darm-compute) | Compute pool over CPUs, GPUs, NPUs and phones: device profiles and live conditions, owner contribution policy, signed short-lived capability adverts (optionally proven to a mesh endpoint by the host's plugin-key binding), smooth-weighted-round-robin shard planning across CPU and GPU with replicas on distinct nodes, measurement-corrected throughput, adaptive shard sizing, signed work receipts and majority tallies that become ledger `ComputeCommit` entries. |
| [`darm-protocol`](crates/darm-protocol) | Polymorphic and adaptive protocols: per-pair negotiation of cipher suite (ChaCha20-Poly1305 vs AES-GCM by hardware), codec (JSON/CBOR) and padding; Noise XX handshakes (`snow`) bound to DARM Ed25519 identities; sealed frames with masked nonces, bucket padding, replay windows and bounded fragmentation; RFC 6298 RTT estimation and AIMD batching tuned by battery, metered and thermal state. |
| [`darm-fabric`](crates/darm-fabric) | The fabric shim: one authenticated link carries many workload channels, each with a sync class (`async`, `block`, `pipeline`, `tight`), size limit and rate limit. Strict priority for `tight` traffic with deadlines, weighted sharing for the rest, bounded queues in both directions. Binary tensor streams (`f32` or quantized `i8`) chunked to one frame each and digest-verified. Carrier-agnostic (`Transport` trait; in-process and TCP carriers included) with an I/O-free link state machine. |
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
`darm_status`, `darm_resources`, `darm_memory_search`, `darm_memory_store`,
`darm_memory_forget`, `darm_memory_outcome`, `darm_memory_feed`,
`darm_ledger_proof`, and the compute pool's
`darm_compute_status`, `darm_compute_conditions`, `darm_compute_plan`,
`darm_compute_receipt`, `darm_compute_tally`. `darm_resources` reports pooled
nodes, CPU cores, GPU/NPU devices and accelerator memory **available** vs
**allocated** to in-flight jobs, plus this node's local contribution.

New here? [`docs/QUICKSTART.md`](docs/QUICKSTART.md) covers running standalone,
bringing your own model or token, and reading the resource view.

## Memory that learns from results

Recalling a memory strengthens it. On its own that rule has a flaw: a memory
that is retrieved often is rehearsed and consolidated whether or not it was
right, so a confidently wrong memory can become one of the strongest. DARM
therefore also records what happened after a recall.

```text
 darm_memory_search ──► hits (each with id, usefulness, outcomes)
        │
        ▼ act on a hit
 darm_memory_outcome { id, helped: true | false }      one report per recall
        │
        ▼
 usefulness = (helped + 2) / (helped + hurt + 4)       starts at 0.5
        ├─ multiplies the memory's recall score by 0.5 + usefulness
        ├─ more hurt than helped  → rehearsal no longer consolidates it
        └─ 4+ reports, below 0.35 → demoted to short-term, left out of recall
```

- **It takes several reports to move a memory.** One bad outcome shifts the
  estimate from 0.5 to 0.4; four in a row with none good are needed before a
  memory is treated as harmful.
- **Feedback must be earned by use.** A report is accepted only for a memory
  with a recall that has not been scored, once per recall, so a memory
  cannot be boosted without being used.
- **Nothing is deleted or rewritten.** A harmful memory keeps its signed
  content, can still be listed for review, and is forgotten by ordinary
  decay and eviction. Ledger finality does not override this node's own
  experience of a memory.
- **Outcomes stay local.** They are stored in this node's memory log and
  are never gossiped: a peer's claim that a memory "helped" would be an easy
  way to promote a poisoned one. Sharing outcomes safely is future work.

`darm_memory_feed { after, limit }` returns the store's recent activity
(remembered, learned from a peer, recalled, helped, hurt, consolidated,
demoted, finalized, forgotten) as ids and event kinds with a sequence number
to resume from. It never includes memory content, and it is an in-memory
view of the last 512 events, not an audit log.

What this is: a feedback loop in which results change what the node
retrieves next. What it is not: the node does not judge outcomes itself (the
caller reports them), it does not retrain any model, and the feed is a record
of memory activity, not awareness.

## Compute pool: CPUs and GPUs, servers and phones

mesh-llm pools GPU memory to serve models. DARM adds a pool for compute
that treats every processor alike, so a mesh can run on GPUs, on CPUs, or on
both at once, and a phone can join and draw on the whole pool:

- Each node signs a **capability advert** every minute (slower on battery
  or metered links): its devices (CPU detected; GPUs declared with
  `DARM_GPUS`), the share its owner contributes, the workloads it serves and
  live conditions. Adverts expire after at most ten minutes, carry a
  sequence number against replay, and are bound to one DARM network. When
  the host supports mesh-llm plugin keys, the advert also carries the host's
  signature binding the DARM key to the node's mesh endpoint, so the mapping
  from a mesh candidate to its hardware is proven, not claimed.
- **Routing**: the `darm` model weights each candidate endpoint by its pooled
  hardware (faster, idler endpoints score higher; phones on battery lower),
  on top of the pheromone trails.
- **Jobs**: `darm_compute_plan` splits a job (embedding, indexing,
  training shards, verification, inference) across CPU, GPU and NPU slots in
  proportion to effective throughput by smooth weighted round robin, or
  restricts it with `cpu_only` / `gpu_only` / `prefer_*`. Each shard gets
  `redundancy` replicas on distinct nodes, preferring different backends;
  `same_kind_replicas` keeps replicas on one device kind when CPU and GPU
  kernels round differently. `threshold` sets the admission quorum,
  `spot_check` adds hidden extra replicas, and `sync_class` applies the
  Locality Doctrine (see [Whitepaper alignment](#whitepaper-alignment)).
  Each assignment carries a suggested shard size so a phone gets small
  shards and a GPU large ones (adaptive sizing toward a target duration).
- **Proof of useful work**: workers sign receipts (`darm_compute_receipt`);
  `darm_compute_tally` accepts only receipts from assigned workers over the
  planned inputs, takes the output reaching the job's threshold, names
  dissenters (evidence for validator reputation reports), feeds measured
  throughput back into the scheduler, and with `commit: true` writes a
  `ComputeCommit` (job, result root, receipt root) to the ledger. This
  replaces the PoC's hash-burning "Proof of Learning" with work that was
  useful and cross-checked.
- **Owner consent on mobile**: phones and tablets contribute by default only
  while charging, above 50% battery, on unmetered networks and not hot; a
  mobile app reports changes through `darm_compute_conditions`. A phone that
  is not contributing still uses the pool.

Nothing in the pool executes remote code: workloads are named kinds that each
operator chose to serve; DARM plans, measures and verifies.

### Sampled audits: checking work without redoing all of it

Redundancy verifies a job by running every shard two or three times. A
sampled audit (`darm_compute::audit`) runs each shard once and then re-runs a
small, unpredictable part:

1. Workers return signed receipts. The coordinator tallies them and makes
   the tally's `receipts_root` public.
2. A beacon that nobody could predict at step 1 becomes known, for example
   the hash of a later ledger block.
3. `plan_audit(job, receipts_root, beacon)` picks the audited shards and one
   auditor each. Anyone can recompute it, so neither the coordinator nor a
   worker chooses what is checked or by whom. A shard's own workers never
   audit it.
4. Auditors re-run and sign; `audit_tally` compares them with the accepted
   outputs.
5. A contradiction goes to more auditors (`escalation`, `resolve`). A
   majority of those auditors is needed either way; silence decides nothing.

| Audit rate | Extra work | Caught if 1 shard is wrong | 10 wrong | 50 wrong |
|---|---|---|---|---|
| 5% | about 5% | 5% | 40% | 92% |
| 10% | about 10% | 10% | 65% | 99.5% |
| 20% | about 20% | 20% | 89% | over 99.99% |

A worker that cheats on `k` shards is caught with probability
`1 - (1 - rate)^k`. That is weak against a single wrong shard and strong
against cheating at scale, which is the case that pays.

Limits, stated plainly: the beacon's block proposer can bias the sample by
trying a few candidate blocks (a verifiable random function would remove
that, and is not implemented); auditors re-run whole shards, so this does not
reach the roughly 1% overhead of schemes that check sampled hidden states;
being caught currently costs reputation only, since there is no stake to
slash; and auditors that share an operator with the worker can confirm a
wrong result. The module is a library today: the plugin's compute operations
do not call it yet.

## Whitepaper alignment

How the DARM-ANN papers map onto this code, and at what epistemic status
(Volume 1 §45 labels: theorem, proposition, engineering estimate,
hypothesis).

| Paper concept | Where | Notes |
|---|---|---|
| CDCP admission threshold τ_c (Paper 1 §3, Vol. 1 §11) | `darm_compute::admission::Threshold`, job `threshold`, `DARM_ADMISSION_THRESHOLD` | Results are admitted at `⌈τ_c·n⌉` of a shard's replicas; short of quorum the shard abstains (`disputed`/`missing`) rather than guessing — ESE's third outcome (Paper 2 §5). |
| Corrected P39 admission bound (Paper 1 §6, Vol. 1 §41) | `darm_compute::admission::admission_probability` | Gaussian-copula integral; the tests reproduce the papers' tables (2.1e-9 … 7.1e-42) and Hypothesis H1's 2.2× (more nodes) vs 17.6× (τ_c 2/3→0.9) at ρ = 0.5. Plan responses report it labelled **modeled**: ρ is assumed, not measured. |
| ρ reduction by diversity (Paper 2, Vol. 1.1 §3) | planner `diverse_replicas`, plan `mean_backend_diversity` | Replicas of a shard prefer different backends. Diversity is reported as an observable proxy, not converted into a ρ value the code cannot know. |
| New-node vote weighting (Vol. 1 §11) | `ComputePool::with_min_age`, `DARM_POOL_MIN_AGE_MS` | Linear ramp; default 10 min rather than the paper's 60 s, per Vol. 1.1 §3's note that 60 s suits only a mature network. |
| Bootstrap security phases (Vol. 1.1 §3.1, BS1) | `ValidatorSet::security_phase`, `darm_status.ledger.security_phase` | The `>2/3` finality rule already equals Phase A's rule (unanimity at 3, N−1 at 4–6; tested). Unsponsored compute workers' trust ceiling rises with the phase (0.1 → 0.3 → 0.5). |
| Redundant spot-execution, P66/P67 (v7.2 §4.3) | job `spot_check`, plan `p67_undetected_fraud_bound` | A keyed hash marks shards for a hidden extra replica; `(1−q)^k` is reported. |
| Locality Doctrine `sync_class` (v7.2 §4.5) | job `sync_class` | `tight` stays on the coordinator, `pipeline` needs measured session RTT ≤ 30 ms, `block`/`async` go anywhere. |
| PoUI / ATTEST receipts (v7.2 §4.3) | `darm_compute::receipt`, ledger `ComputeCommit` | Proof of useful work recorded on the quorum-finalized chain. |
| Signed capability prefixes (v7.2 §2.2) | `darm_compute::advert` | TTL-scoped, replay-protected, optionally bound to the mesh endpoint by the host. |
| Tier routing (Paper 3) | `darm.tier` = `tiny`/`mid`/`consolidator` | Narrows candidates by parameter count when any match; never empties the pool. Speculative decoding across tiers is not implemented (Paper 3 shows it only pays when the mid tier is a real filter). |
| Consensus ≠ truth (Vol. 1 §45) | injected memories carry `status="consensus-validated"` or `"unconfirmed"` | The preamble says finalized memories are system belief, not verified fact. |
| Three-plane separation (Vol. 1.1 §2) | design | Compute operations plan, measure and verify; they never execute code or write model weights. |

Not implemented here, by design or pending evidence: the privacy plane's
onion routing (Vol. 1.1 §4 flags its missing content-policy layer as the
design's largest open gap), token settlement and staking (Vol. 1.1 §9 legal
review items), and a measured ρ for real validators (Paper 2 §6, the
central open experiment).

## Consensus: how a block becomes final

Two rules are implemented. A chain uses one; every validator must use the
same one. `DARM_CONSENSUS` selects it.

**One vote (`DARM_CONSENSUS=one-vote`, the default).** The proposer for
`(height, round)` batches pending entries into a block. Validators attest to
it, at most one block per height each. The block is final once attestations
cover more than two thirds of the validator weight. That rule is safe, but a
proposer that sends block A to half the validators and block B to the rest
splits the vote, and since nobody may vote twice the height never finishes.

**Lock-based BFT (`DARM_CONSENSUS=locking`).** Each height runs rounds
`0, 1, 2, ...` with a rotating proposer, and each round has two votes:

```text
 proposer(h, r) ──proposal──►  validators
                                  │ prevote(block | nil)
                                  ▼
            >2/3 prevotes for one block in this round  = a "polka"
                                  │ lock on it, precommit(block)
                                  ▼
            >2/3 precommits for it in this round        = decided
                                  │
          no polka / no decision ─┴─► timeout ─► round r+1, next proposer
```

- **Voting again is allowed, overturning is not.** A validator that
  precommits a block is locked on it. In later rounds it prevotes nil for
  anything else, unless it is shown a polka for another block from a round
  newer than its lock. So a split vote costs one round instead of the
  height, and a block that any honest validator decided can never lose to a
  different one.
- **Re-proposal.** A proposer that knows of a polka must propose that block
  again, naming the polka's round and attaching the polka itself (the signed
  prevotes). Locked validators verify the certificate and follow.
- **Certificates are checked on their own.** A polka or a decision is a set
  of signed votes carrying more than two thirds of the weight. Receivers
  verify the set directly instead of merging it into their own tally,
  because a Byzantine validator can show different votes to different
  nodes. Nodes that decide relay the commit certificate.
- **Evidence.** Two different signed votes of one kind from one validator in
  one round are returned as an equivocation pair: proof of misbehaviour that
  anyone can check.
- **Bounds.** Votes are accepted only from validators, for this height, at
  most 16 rounds ahead; one vote per validator per round and kind is kept.

What it guarantees, and what it does not:

| | Holds when |
|---|---|
| Two honest validators never decide different blocks | Under one third of the weight is faulty. Independent of timing. |
| Every honest validator eventually decides | The above, plus messages eventually arrive within the caller's timeouts. |
| Neither | A third or more of the weight is faulty. A test demonstrates the break with 2 of 4. |

How the node runs it:

- **Messages.** `round_proposal` (the signed proposal, its block and, for a
  re-proposal, the polka) and `round_vote` travel on `darm.v1`. A finalized
  block is relayed with its commit certificate, so a node that missed the
  votes, or was shown different ones, verifies the decision on its own.
- **The block records how it was decided.** `commit_round` plus that
  round's precommits are the commit certificate, checked on gossip, sync
  and replay. Blocks finalized under the one-vote rule remain valid
  history.
- **Durable votes.** Every vote is written to `votes-<chain>.jsonl` in the data
  directory and synced to disk before it is sent. On restart the node
  restores its lock and will not sign a second vote for a round. A
  validator without a data directory logs a warning: it must not be
  restarted in the middle of a height.
- **Timers.** A round's timer runs only while there is something to agree
  on, and later rounds wait longer (`ROUND_TIMEOUT_MS x (round + 1)`), so
  an idle chain does not burn through rounds and a slow network eventually
  gets a round that is long enough.
- **Equivocation** is counted in `darm_status` and logged. It is not yet
  written to the ledger as slashable evidence.

Switching an existing chain: stop every validator, set
`DARM_CONSENSUS=locking` on all of them, and start them again. Do not switch
while a block is being voted on, and do not run a mixed set: a validator that
signs under both rules at one height counts toward two different quorums.
Nodes on different rules ignore each other's consensus messages. The DARM-ANN
archive server must be on a `darm-core` revision that includes this change
before it can verify blocks finalized by lock-based BFT.

## Sessions: polymorphic and adaptive protocols

With `DARM_SESSIONS=preferred` (default) every node runs a Noise XX
handshake with each peer and seals gossip to peers that completed one:

- **Polymorphic**: each pair negotiates suite, codec and padding from both
  sides' ranked offers (lowest rank sum wins). A server with AES-NI talking
  to another uses AES-GCM, JSON and power-of-two padding; with a phone on
  cellular it lands on ChaCha20-Poly1305, CBOR and light padding. Frames have
  no fixed bytes on the wire: the explicit nonce is masked like QUIC packet
  numbers and plaintexts are padded to buckets.
- **Identity-bound**: each side signs the handshake hash and its Noise static
  key with its DARM Ed25519 identity, so gossip inside a session is
  attributable to a proven node. Both offers are in the authenticated
  transcript, so a man in the middle cannot downgrade the choice.
- **Adaptive**: per-link RFC 6298 RTT estimates set request timeouts; AIMD
  sizes memory batches; battery, metered and thermal state stretch gossip
  intervals, shrink fan-out and switch announcements to pull-only.
- Sessions last at most an hour (fresh keys each time), replay windows and
  reassembly are bounded, and a peer may start one handshake per 5 s.

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
| `DARM_CONSENSUS` | `one-vote` | `one-vote` or `locking` (lock-based BFT rounds; an equivocating proposer costs a round instead of stalling the height). Must be identical on every validator of a chain. Validators need `DARM_DATA_DIR` so votes are durable. See "Consensus" above. |
| `DARM_SESSIONS` | `preferred` | `off`, `preferred` (seal to peers with sessions, plaintext signed gossip to others) or `required` (only handshakes in the clear; plaintext gossip refused). Forced to `required` on a public mesh unless explicitly `off`. |
| `DARM_COMPUTE_POOL` | `1` | Advertise this node's devices and accept peers' adverts. |
| `DARM_FORM_FACTOR` | `server` | `server`, `desktop`, `laptop`, `phone`, `tablet`, `edge`; sets contribution defaults. |
| `DARM_GPUS` | none | Comma-separated `backend:memory_mb:gflops[:label]`, e.g. `cuda:24576:82000:rtx4090`. |
| `DARM_WORKLOADS` | `inference,embedding` | Workload kinds this node serves (`inference`, `embedding`, `indexing`, `training`, `verification`). |
| `DARM_CONTRIBUTE` | form factor | Contribute at all. |
| `DARM_POOL_DEVICES` | `cpu,gpu` | Which device kinds to contribute. |
| `DARM_POOL_SHARE_PCT` | 100 (50 phones) | Share of each device offered. |
| `DARM_POOL_REQUIRE_CHARGING` / `DARM_POOL_MIN_BATTERY_PCT` / `DARM_POOL_ALLOW_METERED` | form factor | Battery and data-plan consent. |
| `DARM_METERED` | `0` | Start in a metered-network state. |
| `DARM_ADMISSION_THRESHOLD` | `2/3` | Default CDCP admission threshold τ_c for compute results (`2/3`, `4/5`, `0.9`; must exceed 1/2). Use ≥ `4/5` for high-risk work or when worker correlation is unknown. |
| `DARM_POOL_MIN_AGE_MS` | 600000 | New-node weight ramp: a peer's scheduling weight grows linearly to full over this time. 0 disables. |
| `DARM_ASSUMED_P_NODE` | 0.01 | Per-worker miss rate assumed by *modeled* guarantees in plan responses (Paper 1's conservative point). An assumption, not a measurement. |

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
  users can read it; secrets are zeroized and never printed. Noise static
  keys live only in memory for the life of the process; each handshake
  re-authenticates them with the identity key.
- **Authenticated sessions.** Noise XX (via `snow`) with identity binding;
  negotiation is downgrade-resistant (offers in the prologue and the
  encrypted transcript, one retry at most); frames are replay-protected and
  the window only moves on authentic frames; a sealed frame can never carry
  another handshake or sealed frame. Public meshes require sessions.
- **Compute integrity.** Adverts are signed, network-bound, short-lived and
  replay-protected; implausible throughput claims are rejected and all
  claims are corrected by measured receipts. Receipts are accepted only from
  workers assigned that shard over the planned input, and results need a
  strict majority of assigned replicas on distinct nodes. Ledger
  `ComputeCommit`s are keyed by (coordinator, job), so copying a job id seen
  in gossip cannot pre-empt its coordinator. Majority voting assumes
  independent replicas; restrict trust for high-value jobs.
- **Rate limits.** Besides per-peer buckets, a node-wide bucket caps inbound
  gossip, because peer ids are claims and can be rotated.

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
cargo test -p darm-fabric
cargo clippy -p darm-core -p darm-memory -p darm-swarm -p darm-mesh-plugin --all-targets -- -D warnings
cargo clippy -p darm-fabric --all-targets -- -D warnings
```

See [`UPSTREAM.md`](UPSTREAM.md) for how the mesh-llm sources were imported
and how to sync them, and [`docs/ROADMAP.md`](docs/ROADMAP.md) for what is
next.
