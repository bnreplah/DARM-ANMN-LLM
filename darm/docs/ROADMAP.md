# DARM-ANN-LLM roadmap

Ordered by security value first.

1. **Peer binding.** Bind each DARM node id to its mesh endpoint id with a
   mutual signature, so reputation reports can target the peers that actually
   served a request, and so gossip can be attributed once mesh-llm signs
   plugin frame origins.
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
