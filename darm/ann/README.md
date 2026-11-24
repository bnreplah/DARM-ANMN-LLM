# DARM-ANN

**Distributed Agentic Resilient Memory over Artificial Neural Networks.**

DARM-ANN gives a network of AI nodes a shared memory that no single node
controls and no node can forge: memories are signed and content-addressed,
committed to a ledger that finalizes by validator quorum, and recalled
associatively into model prompts. Around that memory sits a biomimetic swarm:
nodes have measured health, energy and reputation, lay pheromone trails on
the routes that serve them well, and grow the swarm only through
validator-approved replication.

This directory is the **framework hub**: the protocol specification, the
server and infrastructure anyone can deploy, the on-chain anchor, the node
monitor and the companion app. It lives inside the **DARM-ANN-LLM**
repository (mesh-llm with DARM-ANN built in, `bnreplah/DARM-ANMN-LLM`), next
to the protocol crates in [`../crates`](../crates).

```text
                 ┌──────────── DARM-ANN-LLM mesh (bnreplah/DARM-ANMN-LLM) ─────────────┐
 OpenAI clients ─► mesh-llm node ── darm.v1 gossip (iroh QUIC) ── mesh-llm node ── ...  │
                 │   └ darm plugin: memory · ledger · pheromone routing · swarm         │
                 └─────────────────────────────┬───────────────────────────────────────┘
                                               │ finalized blocks + shared memories (HTTPS)
                                               ▼
                         darm-node archive (darm/ann/server/) ── checkpoints ──► DarmAnchor
                         verifies everything, serves recall,                      (Ethereum,
                         Merkle proofs and replication plans                      contracts/)
```

## What is here

| Path | What it is |
|---|---|
| [`spec/PROTOCOL.md`](spec/PROTOCOL.md) | Normative protocol: canonical encodings, hashing domains, memory, ledger and quorum rules, gossip messages, archive HTTP API. |
| [`spec/test-vectors.json`](spec/test-vectors.json) | Byte-exact vectors an independent implementation must reproduce (`darm-node vectors` regenerates them). |
| [`server/`](server/) | `darm-node`: verifying archive/anchor server (HTTP API), offline `keygen`, test vectors, operator replication planning. |
| [`contracts/`](contracts/) | `DarmAnchor.sol`: anchors ledger checkpoints on Ethereum once >2/3 of validator accounts approve; tests run on an in-process EVM. |
| [`deploy/`](deploy/) | Hardened Dockerfile and compose stack, Kubernetes manifests (restricted PSA, default-deny NetworkPolicy), systemd unit. |
| [`monitor/`](monitor/) | Node monitor: a static, read-only page that draws the network's nodes and block activity as a live graph. Deploys to GitHub Pages. |
| [`companion/`](companion/) | Phone companion: an installable web app (iOS and Android, no developer keys) with its own signed device identity, a locally verified credit wallet and a ledger-audit worker. Deploys to GitHub Pages under `/companion/`. |
| [`scripts/`](scripts/) | Token generation and checkpoint anchoring helpers. |

The original Python proof of concept stays on the
[`../legacy-poc/`](../legacy-poc/) directory, with a concept map.

## Concepts

| Concept | In DARM-ANN |
|---|---|
| Distributed memory | Signed, content-addressed records; `local` (never leaves the node) or `mesh` (gossiped, committed); short-term records decay, recalled/important/finalized ones consolidate to long-term. |
| Blockchain validation | A permissioned ledger: Ed25519-signed entries, blocks final with attestations from >2/3 of validator weight, Merkle roots for light clients, Ethereum anchoring of checkpoints. |
| Swarm intelligence | Pheromone trails per `(peer, model)` route with half-life decay; ant-colony routing with an exploration share for scouts. |
| Biomimetic lifecycle | Germinating → active → flowering → reproducing, resting, damaged → healing, driven by measured success rate, latency and load. |
| Self-replication | Deterministic genomes and offspring; replication is a ledger proposal that validators vote on and an operator executes. |
| Consensus | Ledger proposals and validator votes with the same >2/3 rule. |
| Emotional context | Memory valence and the `affective` memory kind. |

## Quick start

### 1. Run DARM-ANN-LLM nodes

Build and run mesh-llm from [DARM-ANMN-LLM](https://github.com/bnreplah/DARM-ANMN-LLM)
(`just build`, then `mesh-llm serve`). DARM is on by default; every node
advertises a `darm` model. For a shared ledger across nodes, give them the
same chain and validator set:

```bash
darm-node keygen --out ~/.mesh-llm/darm/identity.key   # on each validator, prints its id
export DARM_CHAIN_NAME=my-darm-mesh
export DARM_VALIDATORS=<id1>,<id2>,<id3>
export DARM_SHARE=1
# Optional features, off until scheduled; same value on every validator and
# archive. Pick a height far enough ahead for everyone to upgrade.
# export DARM_ACTIVATE=credits@1200,evidence@1200,governance@1200,pq-identity@1200
mesh-llm serve --model Qwen3-8B-Q4_K_M --join <token>
```

Credits (closed-loop compute payments), equivocation evidence, validator-set
governance and post-quantum identities change the ledger encoding, so they
activate at an agreed height (spec §4.8). Compare
`activations_commitment` in each node's status and `GET /v1/info` before that
height; replicas with different schedules fork there.

### 2. Run the archive node

```bash
scripts/gen-token.sh deploy/docker/secrets/write_token
cp deploy/docker/.env.example deploy/docker/.env   # same chain name and validators
docker compose -f deploy/docker/docker-compose.yml up --build
```

Point mesh nodes at it with `DARM_ARCHIVE_URL=https://archive.example` and
`DARM_ARCHIVE_TOKEN_FILE=/path/to/write_token`. They mirror finalized blocks
and shared memories every 30 seconds; the archive re-verifies all of it.

### 3. Anchor on Ethereum (optional)

Deploy `contracts/src/DarmAnchor.sol`, call `registerChain` with the chain id
(from `GET /v1/info`) and the validators' Ethereum accounts, then each
validator runs `scripts/anchor-checkpoint.sh` to approve the latest
checkpoint.

### 4. Act on replication votes

```bash
darm-node replication-plan --archive https://archive.example --state ./replication.done
```

Lists `replicate` proposals that passed; scale with your own credentials,
then `--mark-done <id>`.

### 5. Watch the network (optional)

[`monitor/`](monitor/) is a static page that polls archive nodes and draws
validators, proposers and memory authors as a graph that pulses as blocks
finalize. It is read-only: no key, no writes. Deploy it to GitHub Pages with
the included workflow and allow its origin on each archive:

```bash
DARM_NODE_CORS_ORIGINS=https://<you>.github.io darm-node serve ...
```

With no archive reachable it shows only itself. See
[`monitor/README.md`](monitor/README.md).

## Security

The design assumes hostile peers and a public network:

- Nothing is accepted on the sender's word: every memory, tombstone, entry,
  block and attestation is signature-checked, domain-separated and bound to
  its chain; stored history is re-verified on every start.
- Write access to the archive needs a 256-bit bearer token (constant-time
  compared) on top of signature checks; the server binds to loopback by
  default and expects TLS termination in front.
- Peer memories reach prompts only from trusted authors, fenced and
  sanitized against indirect prompt injection; private memories never leave
  their node.
- No component replicates itself. Replication is a vote plus an operator.
- Containers run non-root, read-only, with all capabilities dropped;
  Kubernetes manifests enforce the restricted Pod Security Standard and
  default-deny networking; the systemd unit is sandboxed.
- CI uses least-privilege tokens and SHA-pinned actions.

Report vulnerabilities privately via GitHub security advisories on this
repository.

## Development

```bash
cd darm/ann/server && cargo test   # darm-node (path dependency on darm/crates)
cd contracts && npm ci && npm test   # DarmAnchor on an in-process EVM
```

## License

MIT (see [LICENSE](LICENSE)).
