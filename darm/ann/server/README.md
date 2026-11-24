# darm-node

Verifying archive and anchor server for a DARM-ANN chain.

It keeps a non-voting replica of one chain's finalized ledger and the
mesh-scoped memories, re-verifying every signature, quorum and state
transition on write and on startup. It serves recall, Merkle inclusion
proofs, checkpoints for Ethereum anchoring, and the list of passed
`replicate` proposals. The HTTP API is specified in
[`spec/PROTOCOL.md`](../spec/PROTOCOL.md#6-archive-node-http-api-darm-node).

```bash
cargo run -- serve --chain-name my-darm-mesh --validators <id1>,<id2>,<id3> \
  --data-dir ./data --write-token-file ./write_token     # 127.0.0.1:8088
cargo run -- keygen --out ./validator.key                # prints the node id
cargo run -- vectors > ../spec/test-vectors.json
cargo run -- replication-plan --archive http://127.0.0.1:8088 --state ./replication.done
```

## Dependencies

`darm-core` and `darm-memory` are path dependencies on `../../crates`, so the
server always verifies with the protocol code in the same commit. It is its own
Cargo workspace (like `darm/compat-tests`), with its own `Cargo.lock`, so it
builds and locks independently of the mesh-llm workspace.

## Operating notes

- Without `--write-token-file` the node is read-only. Tokens must be at least
  32 characters; `scripts/gen-token.sh` makes 256-bit ones.
- `--read-token-file` additionally gates reads, for archives of private
  meshes.
- Bind to loopback (the default) and terminate TLS in a proxy; never expose
  the plain HTTP port.
- `--cors-origins` (`DARM_NODE_CORS_ORIGINS`, comma-separated) lets web pages
  on the listed origins read the node from a browser, for the
  [node monitor](../monitor/). It is off by default, takes exact origins
  only (no wildcard, no path), allows `GET` only and never credentials, so
  it cannot be used to write. If a read token is set, the page must supply
  it; do not put a read token in a public page.
- The data directory holds verified logs only, no secrets; back it up like
  any database.
