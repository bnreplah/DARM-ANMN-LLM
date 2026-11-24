# DARM-ANN protocol, version 1

This document is normative for anyone implementing DARM-ANN outside the
reference code (`darm-core`, `darm-memory` in
[DARM-ANN-LLM](https://github.com/bnreplah/DARM-ANMN-LLM/tree/main/darm)).
An implementation is conformant when it reproduces every value in
[`test-vectors.json`](test-vectors.json) and enforces every MUST below.

The key words MUST, MUST NOT, SHOULD and MAY are used as in RFC 2119.

## 1. Primitives

| Primitive | Choice |
|---|---|
| Signatures | Ed25519 (RFC 8032). Verifiers MUST use strict verification (reject small-order keys and non-canonical `S`), e.g. `verify_strict` in ed25519-dalek. |
| Node id | The 32-byte Ed25519 public key. Text form: 64 lowercase hex characters. |
| Hash | BLAKE3 in derive-key mode: `H(domain, data) = BLAKE3.derive_key(context = "DARM-ANN/v1 " ‖ domain, data)`. 32 bytes, hex in text form. |
| Signature text form | `"ed25519:" ‖ hex(64-byte signature)`. Other tags are reserved (a hybrid ML-DSA tag is planned); unknown tags MUST be rejected. |

## 2. Canonical encoding

Everything signed or hashed is encoded canonically; JSON is transport only
and MUST NOT be signed. The one exception is the A2A Agent Card of §10,
whose format is fixed by another specification.

- The encoding starts with the domain string `"DARM-ANN/v1/" ‖ domain`,
  written as a `str`.
- `u8`, `u16`, `i16`, `u32`, `i32`, `u64`: fixed-width big-endian
  (two's complement for signed).
- `bool`: one byte, `0x00` or `0x01`.
- `bytes`/`str`: `u32` length, then the bytes (UTF-8 for `str`).
- `fixed32`: the 32 raw bytes.
- `opt_fixed32`: `0x00`, or `0x01` followed by the 32 bytes.
- `str_list`: `u32` count, then each `str`.

Floating point never appears; quantities are scaled to integers
(`*_milli` = value × 1000).

## 3. Memory records

Encoding, domain `memory-record`:

```
fixed32 author ‖ u64 created_ms ‖ u8 kind ‖ u8 scope ‖ u16 importance_milli ‖
i16 valence_milli ‖ str_list tags ‖ str content ‖ opt_fixed32 parent
```

- `kind`: 1 episodic, 2 semantic, 3 procedural, 4 affective.
- `scope`: 1 local, 2 mesh.
- `id = H("memory-record", encoding)`; `signature = Sign(author, encoding)`.

Validity (MUST): content non-blank and ≤ 16 384 bytes; `importance_milli` ≤
1000; `valence_milli` in −1000..=1000; ≤ 16 tags, each 1..=48 bytes of
`[A-Za-z0-9_.:-]`; valid signature by `author`.

Rules:

- A node MUST NOT send `local`-scope records to any peer, and MUST reject
  `local`-scope records received from peers.
- Records created more than 5 minutes in the receiver's future SHOULD be
  rejected.
- Namespaces are tags of the form `ns:<name>`; recall is filtered by
  namespace.

Tombstones, domain `memory-tombstone`:
`fixed32 memory ‖ fixed32 author ‖ u64 timestamp_ms`, signed by `author`.
A tombstone is valid only if `author` is the memory's author. Tombstones win:
a node that holds a tombstone MUST refuse the memory afterwards.

## 4. Ledger

### 4.1 Chain configuration

- `chain_id = H("chain-id", utf8(chain name))`.
- Validator set commitment: domain `validator-set`,
  `u32 n ‖ (fixed32 node ‖ u64 weight)*` sorted ascending by node bytes;
  `commitment = H("validator-set", encoding)`. Weights MUST be ≥ 1.
- Genesis: domain `genesis`, `fixed32 chain_id ‖ fixed32 commitment`;
  `genesis_hash = H("genesis", encoding)`. Block 1's `prev` is the genesis
  hash.

### 4.2 Entries

Domain `ledger-entry`:

```
fixed32 chain ‖ fixed32 author ‖ u64 timestamp_ms ‖ u64 nonce ‖ body
```

| Tag | Body | Encoding after the tag byte |
|---|---|---|
| 1 | NodeRegistration | `str role ‖ fixed32 genome ‖ u32 generation ‖ opt_fixed32 parent` |
| 2 | MemoryCommit | `fixed32 memory ‖ u16 importance_milli` |
| 3 | MemoryRevoke | `fixed32 memory` |
| 4 | ReputationReport | `fixed32 subject ‖ i32 delta_milli ‖ fixed32 evidence` |
| 5 | Proposal | `str kind ‖ fixed32 payload ‖ u64 deadline_ms` |
| 6 | Vote | `fixed32 proposal ‖ bool support` |
| 7 | ComputeCommit | `fixed32 job ‖ fixed32 result_root ‖ fixed32 receipts_root ‖ u32 shards ‖ u16 workers` |
| 8 | CreditGrant | `fixed32 proposal ‖ fixed32 to ‖ u64 amount` |
| 9 | ComputeSettle | `fixed32 job ‖ fixed32 receipts_root ‖ u32 n ‖ (fixed32 worker ‖ u64 amount)^n` |
| 10 | Equivocation | `vote first ‖ vote second`, where `vote = u8 kind (1 prevote, 2 precommit) ‖ u64 height ‖ u32 round ‖ opt_fixed32 block ‖ fixed32 validator ‖ bytes signature` (64-byte Ed25519) |
| 11 | ValidatorSetChange | `fixed32 proposal ‖ u64 effective_height ‖ u32 n ‖ (fixed32 node ‖ u64 weight)^n` |
| 12 | PqKeyBind | `bytes key ‖ bytes proof` (ML-DSA-65: 1952-byte public key, 3309-byte signature; hex strings in JSON) |

`entry_id = H("entry-id", encoding)`; signature by `author` over the
encoding. `role`/`kind` are 1..=64 bytes of `[A-Za-z0-9_.-]`. The nonce is
random so identical bodies have distinct ids. ComputeCommit requires
`1 ≤ shards ≤ 65536` and `workers ≥ 1`.

### 4.3 Blocks

Header, domain `block-header`:

```
fixed32 chain ‖ u64 height ‖ u32 round ‖ fixed32 prev ‖ fixed32 entries_root ‖
u32 entry_count ‖ u64 timestamp_ms ‖ fixed32 proposer ‖ fixed32 validators
```

`block_hash = H("block", header encoding)`.

- Proposer signature over domain `block-proposal`: `fixed32 chain ‖ fixed32 block_hash`.
- Attestation signature over domain `attestation`: `fixed32 chain ‖ u64 height ‖ fixed32 block_hash`.
- `entries_root` is the Merkle root (§4.5) of the entry ids in block order.

A block at height `h` is **valid** when all hold (MUST):

1. `chain` matches; `height = local height + 1`; `prev` = current head hash
   (genesis hash at height 1); `validators` = the set commitment.
2. `timestamp_ms` ≥ previous block's and ≤ local clock + 5 minutes.
3. `proposer = sorted_validators[(height + round) mod n]` and the proposer
   signature verifies.
4. 1 ≤ entries ≤ 512; `entry_count` matches; `entries_root` matches.
5. Every entry verifies, belongs to the chain, is not already finalized, is
   not duplicated in the block, is at most 5 minutes newer than the block,
   and applies under the state rules (§4.4) in order.

A valid block is **final** when its attestations come from distinct
validators whose total weight `w` satisfies `3·w > 2·total_weight`.

**Safety rule (MUST):** a validator attests to at most one block per height.
Chains that use lock-based finality (§4.6) replace this rule and the
attestation certificate with rounds of prevotes and precommits.
Nodes MUST verify every block, signature and quorum when replaying stored
history, and refuse to start on a log that fails.

### 4.4 State rules

Applied in block order with the block's timestamp:

- NodeRegistration: once per node; generation 0 without parent, otherwise
  the parent must be registered and `generation = parent.generation + 1`.
- MemoryCommit: once per memory id.
- MemoryRevoke: only by the committing author.
- ReputationReport: only validators may report; never about oneself;
  |delta| ≤ 1000; reputation (default 1000) is clamped to 0..=5000.
- Proposal: `deadline_ms` must be after the block timestamp; its id is the
  entry id.
- Vote: only validators; once per validator per proposal; before the
  deadline. A proposal **passes** when yes-weight satisfies the quorum
  inequality, is **rejected** once `3·no_weight ≥ total`, and **expires** after
  its deadline otherwise.

- ComputeCommit: once per `(author, job)`. Records are keyed by the
  coordinator as well as the job id: the job id commits to the coordinator
  (§8.3) but the ledger cannot see the job spec, so keying by job alone would
  let an observer front-run a coordinator's commit.

Proposal kinds in use: `replicate` (payload = hash of the replication
recommendation; acted on only by an operator, see the DARM-ANN README).

### 4.5 Merkle trees

- `leaf(x) = H("merkle-leaf", x)`, `node(l, r) = H("merkle-node", l ‖ r)`.
- Build levels pairwise; an unpaired last node is **promoted unchanged** (not
  duplicated).
- Empty tree root: `H("merkle-empty", "")`.
- Proofs list `(side, sibling)` from leaf to root, where `side` says on which
  side the sibling sits.

### 4.6 Lock-based finality (optional mode)

The rule in §4.3 is safe, but a proposer that sends different blocks to
different validators can split the vote so that no block reaches quorum at
that height. A chain MAY instead finalize blocks by the rounds below. Every
validator of a chain MUST use the same mode, and a validator MUST NOT sign
under both modes at one height. In the reference node the mode is selected
by `DARM_CONSENSUS=locking`.

**Messages.** Votes, domain `consensus-vote`:

```
fixed32 chain ‖ u8 kind ‖ u64 height ‖ u32 round ‖ opt_fixed32 block_hash
```

`kind` is 1 for a prevote and 2 for a precommit; an absent `block_hash` is a
vote for nil. Proposals, domain `consensus-proposal`:

```
fixed32 chain ‖ u64 height ‖ u32 round ‖ fixed32 block_hash ‖ bool has_valid_round ‖ u32 valid_round
```

`valid_round` is 0 when `has_valid_round` is false. The proposal is signed by
`sorted_validators[(height + round) mod n]`, the proposer of that round. The
block it names is a §4.3 block whose own `round` is at most the proposal's
round: a block first proposed in round 1 can be proposed again, unchanged, by
the proposer of round 3.

A **polka** for a block in a round is a set of prevotes for it in that round
from distinct validators with `3·w > 2·total_weight`.

**Rules for a validator at round `r` (MUST):**

1. At most one prevote and one precommit per `(height, round)`.
2. *Prevote.* For the round's proposal naming a valid block `B` (§4.3
   rules 1 to 5):
   - without `valid_round`: prevote `B` if the validator is not locked or
     is locked on `B`;
   - with `valid_round = v < r`: wait until a polka for `B` in round `v` is
     held, then prevote `B` if the validator is not locked, is locked on
     `B`, or its lock is from a round `≤ v`.

   In every other case, and on timeout, prevote nil.
3. *Precommit.* On a polka for `B` in round `r`, having validated `B`: lock
   on `(r, B)` and precommit `B`. On a polka for nil, or on timeout,
   precommit nil.
4. *Decide.* Precommits for `B` in one round from distinct validators with
   `3·w > 2·total_weight` decide `B`.
5. *Re-propose.* A proposer that holds a polka for a block MUST propose that
   block, with `valid_round` set to the polka's round, and SHOULD send the
   polka with it.
6. *Advance.* After a precommit timeout, or on seeing votes for a later
   round from validators with `3·w > total_weight`, move to that round.
   Timeouts SHOULD grow with the round number.
7. A validator MUST persist each vote before sending it, and MUST NOT sign a
   second vote for a `(height, round, kind)` after a restart.

Polkas and commit certificates MUST be verified as sets of signatures in
their own right, not by merging them into a local tally: a faulty validator
may have shown the receiver a different vote. Receivers SHOULD accept votes
at most 16 rounds ahead of their own.

**Certificate in the block.** A block finalized this way carries
`commit_round` (JSON, unsigned integer) and its `attestations` are the
precommits of that round: each signature is over the `consensus-vote`
encoding with `kind = 2`, the block's height, `commit_round` and the block
hash. `commit_round` MUST be ≥ the header's `round`. It is not part of the
block hash. A block without `commit_round` is verified by §4.3. Verifiers
MUST accept at most one attestation per validator and no more attestations
than there are validators.

Two different signed votes of one kind from one validator in one
`(height, round)` are proof of misbehaviour. The reference node counts and
logs them; recording them on the ledger is future work.

Agreement (no two honest validators decide different blocks at a height)
holds while faulty weight `f` satisfies `3·f < total_weight`, regardless of
timing. Termination additionally needs messages to arrive within the
timeouts eventually.

### 4.7 Closed-loop compute credits

Tags 8 and 9 (§4.2) define credits that pay workers for committed compute.
They are closed-loop: there is no transfer or redemption entry, so credits
cannot be sold or cashed out through the ledger. Tags 8 and 9 belong to the
feature `credits` and are valid only from its activation height (§4.8).

State: `balance[node]` (u64, default 0), `issued` (u64), the set of consumed
grant proposals, and the set of settled `(coordinator, job)` pairs.

**CreditGrant** (MUST):

- the author is a validator;
- `proposal` names a Proposal with `kind = "credit-grant"` whose `payload =
  H("credit-grant", encoding)`, domain `credit-grant`, `fixed32 to ‖ u64
  amount`, and that has passed the two-thirds validator vote (§4.4);
- `1 ≤ amount ≤ 10^12`; a proposal authorises at most one grant;
- `balance[to]` and `issued` increase by `amount`; an addition that would
  overflow `u64` rejects the entry.

**ComputeSettle** (MUST):

- the author has a finalized ComputeCommit for `job` (§4.2, keyed by
  `(author, job)`), and `receipts_root` equals that commit's;
- `1 ≤ n ≤ 256` and `n` ≤ the commit's `workers`; every `worker` is distinct,
  registered, and not the author; every `amount` is `1..=10^12`;
- the pair `(author, job)` has not been settled;
- `balance[author] ≥ Σ amount`; the sum is moved from the author to the
  workers.

Invariant: `Σ balance = issued` at every height. The ledger cannot verify the
per-worker amounts (work units live in off-ledger receipts); the coordinator
chooses whom to pay, and paying itself through a second identity only moves
its own credits.

### 4.8 Feature activation

Entry tags added after the first release belong to a named feature:

| Feature | Tags |
|---|---|
| `credits` | 8, 9 |
| `evidence` | 10 |
| `governance` | 11 |
| `pq-identity` | 12 |

Each replica has an activation schedule mapping features to heights (mesh:
`DARM_ACTIVATE=credits@1200,evidence@1500`; archive: `--activate`). An entry
of feature `f` is valid in the block at height `h` only if `f` is scheduled
and `h ≥ schedule[f]`. A replica MUST reject a block containing an entry of
an inactive feature, SHOULD refuse such entries at submission (against height
`head + 1`), and MUST NOT propose them. An unscheduled feature never
activates, so a feature is off by default.

The schedule is not part of the genesis hash, so a running chain can
schedule an upgrade. Replicas with different schedules fork at the first
height where they disagree. To make that visible before it happens, nodes
expose `activations_commitment = H("activations", u32 n ‖ (bytes name ‖ u64
height)^n)` with features in name order (byte order: `credits`, `evidence`, `governance`, `pq-identity`).
Validators MUST compare it before the earliest scheduled height.

### 4.9 Equivocation evidence

Tag 10 records that a validator signed two different votes (§4.6) of the same
kind in the same height and round. Any node may author it.

Shape (MUST): `first` and `second` share `validator`, `kind`, `height` and
`round`; `first.block < second.block` under the order nil < any hash, hashes
compared as bytes (so the votes differ and each offence has one encoding).

Context (MUST): both signatures verify against `vote_message(chain_id, …)` of
this chain, and the feature `evidence` is active.

State (MUST): `validator` is in the current validator set; the key
`(validator, height, round, kind)` has not been recorded, otherwise the
entry is a duplicate; `reputation[validator]` decreases by 1000 milli-units,
clamped to the reputation range.

Recording does not remove the validator; that requires a validator-set
change through governance.

### 4.10 Validator-set governance

The validator set in force for the block at height `h` is the set of the
latest change whose `effective_height ≤ h`, or the genesis set (§4.1) if
there is none. Every rule that refers to "the validators" (block header
`validators` commitment, proposer schedule, attestations and their quorum,
§4.6 votes and certificates, proposal outcomes, the authorship checks of
§4.7 and §4.9) uses the set in force for the block being checked.

Tag 11 (feature `governance`, §4.8):

- Shape (MUST): `1 ≤ n ≤ 128`; members strictly ascending by node id; every
  weight ≥ 1; the weights form a valid validator set (§4.1).
- Context (MUST): `effective_height ≥ h + 2`, where `h` is the height of the
  block containing the entry, so every replica knows the set before
  `effective_height` starts.
- State (MUST): the author is in the set in force; `proposal` names a
  Proposal with `kind = "validator-set"` and `payload = H("validator-set",
  u64 effective_height ‖ u32 n ‖ (fixed32 node ‖ u64 weight)^n)` (the same
  encoding as the entry's tail), which has passed under the set in force;
  the proposal has not been enacted before; and `effective_height` is
  greater than every previously scheduled change's.

A change removes nothing retroactively: blocks below `effective_height`
keep their certificates. Responding to recorded equivocation (§4.9) with a
change is a governance decision, not automatic.

### 4.11 Post-quantum identities

A node MAY hold an ML-DSA-65 key (FIPS 204) in addition to its Ed25519
identity. Throughout this specification an ML-DSA signature "under domain
`d` over `m`" is the FIPS 204 signature, with an empty context string, over
`str "DARM-ANN/v1/" ‖ d` (length-prefixed as every domain tag, §2) followed
by `bytes m`. Signers SHOULD derive the key seed as
`H("pq-identity-seed", ed25519_seed)` so that the two keys share one secret.

Tag 12 (feature `pq-identity`, §4.8) binds the author's key:

- Shape (MUST): `key` is 1952 bytes and `proof` 3309 bytes.
- Context (MUST): `key` is the canonical encoding of an ML-DSA-65 public key
  (re-encoding the decoded key yields the same bytes), and `proof` verifies
  under `key`, domain `pq-key-bind`, over `str "DARM-ANN/v1/pq-key-bind" ‖
  fixed32 chain_id ‖ fixed32 author`.
- State (MUST): the author is registered or in the validator set in force,
  and has no key bound yet. The binding is permanent.

The binding entry is authorised by the author's Ed25519 signature, so it
protects only nodes that bind before Ed25519 forgery is practical.

## 5. Mesh gossip (`darm.v1`)

DARM-ANN-LLM nodes exchange JSON messages (`content_type`
`application/json`) on the mesh-llm plugin channel `darm.v1`. Frames are not
origin-authenticated by the transport, so receivers MUST verify every
contained signature and MUST NOT trust the claimed sender beyond replies and
rate limiting.

| `type` | Fields | Limit |
|---|---|---|
| `memory_announce` | `ids` | ≤ 512 |
| `memory_request` | `ids` | ≤ 512 (senders request ≤ 64) |
| `memory_push` | `memories` | ≤ 64 (senders push ≤ 24) |
| `tombstone` | `tombstone` | |
| `ledger_entry` | `entry` | |
| `block_proposal` | `block` (may carry attestations) | |
| `attestation` | `block_hash`, `attestation` | |
| `round_proposal` | `message`: `proposal`, `block`, `polka` (§4.6) | polka ≤ 256 votes |
| `round_vote` | `vote` (§4.6) | |
| `sync_request` | `chain`, `from_height` | |
| `sync_blocks` | `blocks` | ≤ 32 |
| `compute_advert` | `advert` (§8.1) | |
| `handshake` | `handshake` (§9) | ≤ 8 KiB of Noise payload |
| `sealed` | `frame` (base64, §9.3) | ≤ 87 392 characters |

Bodies over 1 MiB MUST be dropped before parsing. Receivers SHOULD rate-limit
per peer (reference: bucket of 200, refill 50/s). Memory gossip is only
enabled when the node shares memories, and is off on public meshes unless
explicitly enabled. Receivers SHOULD also apply a node-wide bucket (reference:
2000, refill 500/s), because claimed peer ids can be rotated. Compute adverts
and handshakes travel only between direct peers and are never relayed.

## 6. Archive node HTTP API (`darm-node`)

| Method | Path | Auth | Purpose |
|---|---|---|---|
| GET | `/healthz` | none | liveness |
| GET | `/v1/info` | read | chain, validators, head, memory stats |
| GET | `/v1/ledger/blocks?after=H&limit=N` | read | finalized blocks with height > H (N ≤ 64) |
| POST | `/v1/ledger/blocks` | write | append finalized blocks (fully verified) |
| GET | `/v1/ledger/entries/{id}/proof` | read | Merkle proof for a finalized entry |
| GET | `/v1/ledger/checkpoint` | read | latest `(chain_id, height, block_hash, entries_root)` for anchoring |
| GET | `/v1/proposals?kind=&outcome=` | read | ledger proposals and outcomes |
| GET | `/v1/memories?limit=N` | read | mesh memory inventory |
| POST | `/v1/memories` | write | store signed mesh-scoped memories (≤ 64) |
| GET | `/v1/memories/{id}` | read | one signed memory |
| POST | `/v1/memories/search` | read | recall (`{"query", "limit", "finalized_only"}`) |
| POST | `/v1/tombstones` | write | apply a signed tombstone |

`read` is open unless a read token is configured; `write` always requires
`Authorization: Bearer <token>`. Request bodies are capped at 2 MiB.

## 7. Ethereum anchoring

[`contracts/src/DarmAnchor.sol`](../contracts/src/DarmAnchor.sol) stores
`(height, block_hash, entries_root)` per chain once strictly more than two
thirds of the chain's registered validator accounts call
`approveCheckpoint` with identical values. Inclusion of an entry is proven
off-chain with a §4.5 proof against an anchored `entries_root`.

## 8. Compute pool

Nodes pool CPUs, GPUs and NPUs (including phones) for named workloads.
Enum codes used below:

| Field | Codes |
|---|---|
| device kind | 1 cpu, 2 gpu, 3 npu |
| backend | 1 cpu, 2 cuda, 3 rocm, 4 metal, 5 vulkan, 6 other |
| form factor | 1 server, 2 desktop, 3 laptop, 4 phone, 5 tablet, 6 edge |
| thermal | 0 nominal, 1 fair, 2 serious, 3 critical |
| workload | 1 inference, 2 embedding, 3 indexing, 4 training, 5 verification |
| device preference | 0 any, 1 cpu_only, 2 gpu_only, 3 prefer_gpu, 4 prefer_cpu |
| sync class | 0 async, 1 block, 2 pipeline, 3 tight (v7.2 §4.5 Locality Doctrine) |

### 8.1 Capability adverts

Domain `compute-advert`:

```
fixed32 node ‖ fixed32 network ‖ u64 seq ‖ u64 issued_ms ‖ u64 ttl_ms ‖ u8 form_factor ‖
u32 n ‖ n × (u8 kind ‖ u8 backend ‖ str label ‖ u32 memory_mb ‖ u32 compute_units ‖
             u32 gflops ‖ bool unified_memory ‖ u8 share_pct) ‖
u32 m ‖ m × u8 workload ‖
bool on_battery ‖ bool charging ‖ u8 battery_pct ‖ bool metered ‖ u8 thermal ‖ u16 load_milli ‖
(u8 0 | u8 1 ‖ str mesh_peer) ‖
(u8 0 | u8 1 ‖ str plugin ‖ bytes binding_signature)
```

Signed by `node`. Receivers MUST check: `network` is theirs (the chain id
when the chain is named, otherwise `H("network", "default")`); 5 s ≤ ttl ≤
10 min; `issued_ms` ≤ now + 2 min and `issued_ms + ttl_ms` > now; ≤ 16
devices, each with gflops in 1..=2 000 000, `share_pct` in 1..=100 and a
backend matching its kind; 1..=8 distinct workloads; `battery_pct` ≤ 100,
`load_milli` ≤ 1000; `mesh_peer` 1..=128 bytes of `[A-Za-z0-9_.-]`. An advert
replaces a stored one from the same node only with a higher `seq`.

`mesh_binding`, when present, is the mesh-llm host's plugin-key binding:
an Ed25519 signature by the mesh endpoint key `mesh_peer` over

```
"mesh-llm-plugin-key-v1:" ‖ u64le-len ‖ endpoint ‖ u64le-len ‖ plugin ‖
u64le-len ‖ node ‖ u64le-len ‖ "ed25519"
```

A valid binding makes the endpoint → node mapping proven. Without it, a
receiver maps `mesh_peer` only when the advert arrived from that peer, and an
unproven claim never displaces a proven one.

### 8.2 Planning (informative)

Slot weight = `gflops × share × efficiency_correction × reliability × trust ×
age_weight × availability × preference × mobile_factor`; shards are dealt by
smooth weighted round robin; replicas of a shard go to distinct nodes and,
with `diverse_replicas`, a candidate sharing a backend with an already
placed replica has its weight multiplied by 0.25. `age_weight =
min(1, (now − first_seen)/t_min_age)` (reference 10 minutes; the local node
is exempt). Unmeasured
nodes start at efficiency 0.75; measured efficiency is clamped to 0.01..=4×
the pool mean for that device kind.

### 8.3 Jobs and receipts

Job id: `H("compute-job", enc)` where `enc` uses domain `compute-job`:

```
fixed32 coordinator ‖ fixed32 network ‖ u8 workload ‖ u8 device_preference ‖
fixed32 input_root ‖ u32 shards ‖ u8 redundancy ‖ u32 min_memory_mb ‖
bool allow_mobile ‖ bool same_kind_replicas ‖ u16 threshold_num ‖ u16 threshold_den ‖
u16 spot_check_milli ‖ bool diverse_replicas ‖ u8 sync_class ‖ u64 nonce
```

The admission threshold `τ_c = threshold_num / threshold_den` MUST satisfy
`1/2 < τ_c ≤ 1` (default 2/3) so that at most one output per shard can reach
quorum. `spot_check_milli` ≤ 1000; shard `s` carries one extra replica when
`u16be(H("compute-spot-check", enc)[0..2]) mod 1000 < spot_check_milli`, where
`enc` uses domain `compute-spot-check`: `fixed32 input_root ‖ u64 nonce ‖ u32 s`.
`tight` work runs only on the coordinator; `pipeline` work only on workers
with measured RTT ≤ 30 ms.

`input_root` is the Merkle root (§4.5) of the shard input hashes.

Receipt, domain `compute-receipt`, signed by `worker`:

```
fixed32 job ‖ u32 shard ‖ fixed32 worker ‖ fixed32 input ‖ fixed32 output ‖
u8 device ‖ u8 backend ‖ u32 units ‖ u32 elapsed_ms ‖ u64 completed_ms
```

`receipt_id = H("compute-receipt-id", encoding)`. A tally counts a receipt only
when it is for the job, the worker was assigned that shard, and `input`
matches the planned input; one per (shard, worker). An output is agreed when
at least `⌈τ_c·n⌉` of the shard's `n` *assigned* replicas (spot checks
included) signed it; otherwise the shard is `disputed` or `missing` and no
commit is possible. Agreement is consensus-validated system belief, not
ground truth. When every
shard is agreed the coordinator MAY commit `ComputeCommit` with
`result_root` = Merkle root of agreed outputs in shard order and
`receipts_root` = Merkle root of counted receipt ids ordered by (shard,
worker).

## 9. Sessions

Optional authenticated sessions per peer pair on `darm.v1`.

### 9.1 Negotiation

Each side offers ranked lists (1..=8 distinct items each) of suites
(`noise_xx_cha_cha_poly` = `Noise_XX_25519_ChaChaPoly_BLAKE2s`,
`noise_xx_aes_gcm` = `Noise_XX_25519_AESGCM_SHA256`, and the hybrid
post-quantum suites of §9.4: `hybrid_ml_kem768_cha_cha_poly`,
`hybrid_ml_kem768_aes_gcm`), codecs (`json`,
`cbor`) and paddings (`power_of_two` min 128, `block256`, `minimal` = 16).
For each list the selection is the common item with the lowest
`initiator_rank + responder_rank`, ties to the initiator's order.

### 9.2 Handshake

```
I → R  init  { sid (16 bytes hex), offer, suite, noise: msg1 }
R → I  retry { sid, offer }                  when negotiate(offer_i, offer_r).suite ≠ suite
R → I  resp  { sid, noise: msg2(payload = JSON { offer, bind }) }
I → R  fin   { sid, noise: msg3(payload = JSON { bind }) }
```

Prologue, domain `session-prologue`: `fixed32 network ‖ bytes sid ‖ str
suite_label ‖ offer_i`, where `suite_label` is the Noise protocol name for
the classical suites and the label of §9.4 for hybrid ones, and an offer encodes as `u32 n ‖ n × u8` for its
*classical* suites only (1 chacha, 2 aes; hybrid suites are omitted, see
§9.5), codecs (1 json, 2 cbor) and paddings (1 pow2, 2 block256,
3 minimal). The initiator MUST abort unless negotiation over both
authenticated offers yields the suite in use, and accepts at most one retry.

The initiator opens with its first classical suite unless it has completed
a hybrid session with this peer before, in which case it MAY open with its
first suite (§9.5).

`bind = { node, signature }`, signature by `node` over domain `session-bind`:
`fixed32 network ‖ bytes sid ‖ u8 role (1 initiator, 2 responder) ‖ bytes h ‖
fixed32 static_key`, where `h` is the Noise handshake hash just before the
signer writes its static key (responder: after reading msg1; initiator:
after reading msg2). Receivers verify against the Noise remote static key.

`bind` MAY carry `pq`: the base64 ML-DSA-65 signature (§4.11) under domain
`session-bind-pq` over the same message. A node whose chain schedules
`pq-identity` SHOULD always send it, whatever its ledger height. A receiver
whose ledger has a key bound for `bind.node` MUST reject the handshake
unless `pq` is present and verifies under that key; otherwise it ignores
`pq`. Handshake messages are at most 20 KiB (8 KiB before this rule).

### 9.3 Frames

`frame = masked_nonce (8) ‖ noise_ciphertext` with explicit 64-bit nonces.
`mask = BLAKE3-keyed(hp_key, ciphertext[0..16])[0..8]`, `hp_key =
BLAKE3-derive-key("DARM-ANN/v1 header-protection i2r" | "… r2i",
handshake_hash)`. Plaintext: `u32 msg_id ‖ u16 frag_index ‖ u16 frag_count ‖
u32 len ‖ data ‖ zero padding` (padding per the negotiated policy, capped at
65 519). The inner message is a `darm.v1` message other than `handshake` or
`sealed`, encoded with the negotiated codec. Receivers keep a 1024-nonce
replay window updated only after authentication, at most 8 partial messages
per session, 30 s reassembly, 1 MiB per message; sessions end after one hour.

Modes: `preferred` (seal to peers with a session, plaintext to others),
`required` (only handshakes in the clear; plaintext refused; mandatory on
public meshes), `off`.

### 9.4 Hybrid post-quantum suites

A hybrid suite adds an ML-KEM-768 (FIPS 203) exchange so that session keys
depend on both X25519 and ML-KEM. Noise protocols and prologue labels:

| Suite | Noise protocol | `suite_label` |
|---|---|---|
| `hybrid_ml_kem768_cha_cha_poly` | `Noise_XXpsk3_25519_ChaChaPoly_BLAKE2s` | `Noise_XXpsk3_25519+MLKEM768_ChaChaPoly_BLAKE2s` |
| `hybrid_ml_kem768_aes_gcm` | `Noise_XXpsk3_25519_AESGCM_SHA256` | `Noise_XXpsk3_25519+MLKEM768_AESGCM_SHA256` |

- The initiator generates a fresh ML-KEM-768 key pair per attempt and sends
  the 1184-byte encapsulation key as the payload of Noise message 1.
  Classical suites MUST carry an empty message-1 payload; the responder
  rejects any other.
- The responder MUST validate the key as FIPS 203 requires (modulus check),
  encapsulates, and returns the 1088-byte ciphertext, base64, as `kem` in the
  message-2 payload (`{ offer, bind, kem }`). `kem` MUST be present exactly
  for hybrid suites.
- Both sides compute `psk = H("session-hybrid-psk", shared_secret)` and set
  it as the Noise `psk3` (mixed after the initiator's static key). ML-KEM does
  not report a bad ciphertext; a mismatch yields a different key and the
  handshake then fails authentication.
- Message-1 and message-2 payloads are covered by the handshake hash that
  each side signs in its `bind`, so altering the key or ciphertext fails.
- Offers: `preferred` lists hybrid suites before their classical
  counterparts; `required` lists only hybrid suites. The wire form of §9.5
  keeps such offers parseable by implementations that predate this section.
- Authentication stays classical (Ed25519 identities, X25519 statics): this
  protects recorded traffic from later quantum decryption, not a live
  quantum impersonator.

### 9.5 Offer wire form and compatibility

An offer is JSON `{ suites, suites_ext?, codecs, paddings }`:

- `suites` lists only the classical suites (`noise_xx_*`) in preference
  order. Every implementation can parse it.
- `suites_ext`, present only when the full preference list differs from
  `suites`, lists all suites in preference order. Receivers that know it
  MUST use it instead of `suites`; older receivers ignore it as an unknown
  field.
- Receivers MUST skip suite, codec and padding names they do not recognise
  (then apply the 1..=8 non-empty, no-duplicates rule to what remains),
  so later additions do not break parsing.

Because the prologue commits only to the classical suites, an older
implementation computes the same prologue as a newer one. Removing or
editing `suites_ext` in transit changes negotiation only; the initiator
detects that when it re-runs negotiation over the responder's offer, which
arrives authenticated inside message 2, and aborts.

Interoperation (tested against the pre-hybrid release):

| Initiator | Responder | Result |
|---|---|---|
| `preferred` | older | classical session |
| older | `preferred` | classical session |
| `preferred` | `preferred` | opens classical, `retry`, hybrid session |
| `required` | older | no session |

Implementations of the first hybrid release, which listed hybrid suites in
`suites` and committed them in the prologue, cannot initiate to newer ones
while running `preferred` or `required`.

## 10. A2A Agent Cards

A node MAY publish an Agent2Agent (A2A v1.0) Agent Card signed with its
identity, following section 8.4 of the A2A specification:

- Payload: the card without `signatures`, canonicalized with RFC 8785.
  Signers MUST NOT emit null or empty optional fields. Numbers other than
  integers of magnitude at most 2^53 − 1 are not permitted.
- Signature: JWS (RFC 7515) with protected header `{"alg":"EdDSA",
  "kid": <node id, hex>, "typ":"JOSE"}` (optionally `jku`), serialized with
  RFC 8785; the signature is Ed25519 over `BASE64URL(protected) ‖ "." ‖
  BASE64URL(payload)`. Verifiers accept `alg` `EdDSA` or `Ed25519`.
- The card carries the extension `urn:darm-ann:agent-identity:v1` in
  `capabilities.extensions` with `params = { nodeId, chainId }`.
- Verification: the `kid` is the public key. A verifier MUST NOT fetch
  `jku`. A card is vouched for by a chain when a valid signer is registered
  or a validator on that chain and the extension names that signer and that
  chain. Verifiers MAY retry with nulls, empty arrays and empty objects
  removed from the received card.

A JWS signing input is ASCII text, and every other message a node signs
begins with a binary length prefix (§2), so the two cannot be confused.

## 11. Device companions

A device companion is a phone (or any browser) that holds its own identity,
verifies the ledger, and logs signed verification work with an archive. The
archive records and reports; it never pays. Credits move only through ledger
entries (§4.7), so a device's balance is something the device can re-derive
from finalized blocks without trusting the archive.

**Identity.** The device generates an Ed25519 key. Its id is the public key,
which is exactly a `NodeId`, so every request, receipt and ledger entry the
device makes names the same 32 bytes. There is no account and no bearer
secret for device actions.

**Messages.** Each is a canonical encoding (§2) with its own domain tag and
binds the chain id, so a signature cannot be replayed on another chain or
used for another purpose. Signatures are Ed25519 in the tagged form of §1
and are checked with `verify_strict` semantics (small-order keys rejected).

| Domain | Fields after `fixed32 chain ‖ fixed32 node` |
|---|---|
| `device-enroll` | `str platform ‖ str label ‖ u64 created_ms` |
| `device-request` | `str method ‖ str target ‖ u64 timestamp_ms ‖ bytes nonce(16) ‖ fixed32 body_hash` |
| `device-work` | `str kind ‖ u64 from_height ‖ u64 to_height ‖ fixed32 head ‖ u8 verdict ‖ u32 elapsed_ms ‖ u64 completed_ms` |

`body_hash` is the BLAKE3 derive-key hash (§1) of the raw request body under
the context `device-body`. `verdict` is 1 `ok` or 2 `invalid`. `platform` is
one of `ios`, `android`, `web`, `desktop`, `other`; labels and platforms are
limited to ASCII letters, digits, `_`, `-`, `.` (labels also allow spaces)
and 32 bytes.

**Enrollment** (`POST /v1/devices`) is self-signed proof of possession, with
`created_ms` within 120 s of the archive's clock. Repeating the same
enrollment is idempotent; a different claim for an enrolled key is a
conflict. An operator can require an enroll token (`Authorization: Bearer`)
so a public archive is not open to bulk sign-ups.

**Signed requests** carry `X-DARM-Node`, `X-DARM-Timestamp`,
`X-DARM-Nonce` (32 hex chars) and `X-DARM-Signature`. The archive requires
an enrolled node, a timestamp within 120 s, a nonce not seen from that node
within 240 s, and a valid signature over the exact method, request target
(path and query) and body. Authentication happens before the body is parsed.

**Receipts** (`POST /v1/devices/{node}/receipts`, signed request) record a
`ledger-verify` claim over blocks `from_height..=to_height` (at most 10 000
blocks). An `ok` claim must match the archive's block hash at `to_height`.
Receipts are kept in an append-only device log and are listed by
`GET /v1/devices/{node}/receipts`.

| Method | Path | Auth | Purpose |
|---|---|---|---|
| POST | `/v1/devices` | self-signed (+ optional enroll token) | enroll |
| GET | `/v1/devices/{node}` | read | enrollment record |
| GET | `/v1/devices/{node}/wallet` | read | totals the archive derives, registration, reputation |
| GET | `/v1/devices/{node}/activity?before=H&limit=N` | read | grants and settlements involving the device |
| GET | `/v1/devices/{node}/receipts` | read | the device's signed receipts |
| POST | `/v1/devices/{node}/receipts` | signed request | submit a receipt |

**Light verification.** A companion pins the chain name and genesis
validators on first use (the user compares the genesis hash with one the
operator published), then accepts a block only when its hash chain, validator
commitment, designated proposer, entries root, entry signatures and finality
certificate (attestations, or precommits of `commit_round` on lock-based
chains) all verify, tracking `ValidatorSetChange` entries as it goes. It does
not re-run the state rules of §4.4; it relies on the validator quorum for
those, so it is a light client, not a full node. Balances are folded from
verified entries: granted + settled in − settled out.

Reference vectors are in `test-vectors.json` under `companion`: a 12-block
chain with a validator-set change at height 12, the expected balances, and
the device messages. `companion/test/` checks the JavaScript port byte for
byte against them.
