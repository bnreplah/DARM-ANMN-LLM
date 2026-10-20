# DARM-ANN-LLM roadmap

Ordered by security value first.

1. **Peer binding (partly done).** Sessions now prove the DARM identity at
   the other end of a peer link, and compute adverts carry the mesh host's
   plugin-key binding of DARM key to endpoint. Remaining: use the proven
   mapping to target reputation reports at the endpoints that served a
   request.
2. **Lock-based BFT.** Upgrade the one-vote-per-height rule to
   Tendermint/HotStuff-style locking so a Byzantine proposer that equivocates
   cannot stall a height.
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
9. **Shard dispatch.** A `darm.v1` job channel that delivers planned shards
   to workers' registered executors and collects receipts automatically
   (today the coordinator dispatches and gathers receipts itself).
10. **GPU discovery.** Fill the compute profile from mesh-llm's hardware
    survey instead of `DARM_GPUS`.
11. **Mobile SDK hooks.** Call `darm_compute_conditions` from the Swift and
    Kotlin SDKs on power, network and thermal notifications.
12. **Post-quantum sessions.** A hybrid ML-KEM suite once `snow` supports it,
    negotiated through the same polymorphic offer.
