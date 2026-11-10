# DARM-ANN-LLM roadmap

Ordered by security value first.

1. **Peer binding (partly done).** Sessions now prove the DARM identity at
   the other end of a peer link, and compute adverts carry the mesh host's
   plugin-key binding of DARM key to endpoint. Remaining: use the proven
   mapping to target reputation reports at the endpoints that served a
   request.
2. **Lock-based BFT (wired, opt-in).** `DARM_CONSENSUS=locking` finalizes
   blocks by Tendermint-style prevote/precommit rounds with locking, durable
   votes, and commit certificates carried in the block, so an equivocating
   proposer costs a round instead of stalling the height. Remaining:
   - wire encodings and test vectors for `round_proposal`, `round_vote` and
     `commit_round` in DARM-ANN `spec/PROTOCOL.md`, and re-pin the archive
     server's `darm-core` so it can verify these blocks;
   - record equivocation evidence as a ledger entry;
   - make `locking` the default once the above has run on a real mesh;
   - a chain-level activation height, so a switch cannot be done
     inconsistently by operators.
3. **Validator-set governance.** `Proposal { kind: "validators" }` that, once
   passed, schedules a validator-set change at a future height.
4. **Hybrid post-quantum signatures.** Add `Signature::Ed25519MlDsa65` (both
   must verify) behind the existing algorithm tag.
5. **Model embeddings.** An `Embedder` backed by an embedding model served on
   the mesh, with the hashing embedder as fallback.
6. **Encrypted mesh-scoped memories.** Per-namespace symmetric keys
   distributed to authorized nodes, so shared memories can be confidential
   from other mesh members.
7. **Console page.** A web UI projection (`placement = "primary"`) for the
   ledger, memories, pheromone map and proposals.
8. **Anchoring.** Periodic checkpoints of `(height, block hash)` to the
   DARM-ANN Ethereum anchor contract via the archive node.
9. **Shard dispatch.** A job channel that delivers planned shards to
   workers' registered executors and collects receipts automatically (today
   the coordinator dispatches and gathers receipts itself). `darm-fabric`
   now provides the carrier for this: the `compute` channel, the `Workload`
   trait and tensor streams. Remaining: a fabric carrier over the mesh-llm
   plugin channel, an async endpoint, and the executor workload itself.
10. **GPU discovery.** Fill the compute profile from mesh-llm's hardware
    survey instead of `DARM_GPUS`.
11. **Mobile SDK hooks.** Call `darm_compute_conditions` from the Swift and
    Kotlin SDKs on power, network and thermal notifications.
12. **Post-quantum sessions.** A hybrid ML-KEM suite once `snow` supports it,
    negotiated through the same polymorphic offer.
13. **Fabric carriers with independent streams.** Priority in `darm-fabric`
    acts at the sender's queue; on an ordered carrier (TCP) bytes already
    sent cannot be overtaken. A QUIC carrier with one stream per sync class
    removes that head-of-line blocking.
14. **Fabric in CI.** `darm-ci.yml` still lists only the original four
    crates; `darm-compute`, `darm-protocol` and `darm-fabric` are not yet
    gated there.
