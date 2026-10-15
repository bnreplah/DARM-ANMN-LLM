# DARM-ANN proof of concept (legacy, reference only)

These files are the original DARM-ANN proof of concept, copied verbatim from
the `code` branch of `bnreplah/DARM-ANN` at
`045d942b834732ffbd93a01309bc37bfb9a73ba6`. They document where the concepts
came from and are **not built, tested or deployed** by this repository.

> **Do not deploy this code.** Its cryptography is simulated: "encryption" is
> an XOR stream with a derived key, `verify_signature` recomputes an HMAC with
> the *verifier's own* private key (so it cannot verify anyone else), and the
> self-replication engine creates Kubernetes pods and Docker containers
> autonomously. The production implementation is in `darm/crates/`, which
> replaces each of these with real signatures, quorum finality and
> vote-gated replication.

Concept map:

| PoC file / class | Implemented in |
|---|---|
| `QuantumCrypto`, `MPIMessage` signatures | `darm-core::identity` |
| `Block`, `Blockchain`, `mine_block` | `darm-core::ledger` |
| `NodeState`, `BiomimeticNode.lifecycle` | `darm-swarm::lifecycle` |
| `Pheromone`, pheromone propagation | `darm-swarm::pheromone` |
| `NodeGenome`, `SelfReplicationEngine` | `darm-swarm::genome` |
| `MemoryNode`, short/long-term memory, IPFS memories | `darm-memory` |
| `EmotionalNode` | memory `valence_milli`, `MemoryKind::Affective` |
| `ConsensusEngine`, on-chain proposals/votes | ledger `Proposal`/`Vote` entries |
| `DistributedConsciousness` task routing | `darm-mesh-plugin::chat` |
| `BlockchainExplorer`, `web_interface.py` | `darm_status` operation; DARM-ANN archive node API |
| Solidity `DistributedConsciousness` contract | DARM-ANN `contracts/DarmAnchor.sol` |
