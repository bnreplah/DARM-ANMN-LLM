# Upstream provenance

DARM-ANN-LLM incorporates three code bases:

| Source | Imported as | Revision |
|---|---|---|
| [`bnreplah/mesh-llm`](https://github.com/bnreplah/mesh-llm) (fork of [`Mesh-LLM/mesh-llm`](https://github.com/Mesh-LLM/mesh-llm)) | repository root (`mesh/`, `skippy/`, `tools/`, workspace files) | `3db50f429b8080895a49fa10535453832c3f42c8` |
| [`bnreplah/DARM-ANN`](https://github.com/bnreplah/DARM-ANN) `code` branch | `darm/legacy-poc/` (verbatim, reference only) | `045d942b834732ffbd93a01309bc37bfb9a73ba6` |
| DARM-ANN concepts, re-implemented | `darm/crates/` | new |

The mesh-llm tree was imported as a snapshot (`git archive` of the revision
above) rather than with its history, so this repository does not carry a
shallow upstream graph.

## Local changes to upstream files

Kept deliberately small so syncing stays mechanical:

- `Cargo.toml`: the four `darm/crates/*` workspace members.
- `mesh/crates/mesh-llm-host-runtime/Cargo.toml`: `darm-mesh-plugin` dependency.
- `mesh/crates/mesh-llm-host-runtime/src/plugin/mod.rs`: `DARM_PLUGIN_ID`, `darm_builtin_enabled()`.
- `mesh/crates/mesh-llm-host-runtime/src/runtime/run_auto.rs`: reserve the `darm` plugin name and register the optional in-process built-in.
- `Cargo.lock`: the new crates.
- `mesh/deploy/docker/Dockerfile.client`, `mesh/deploy/fly/Dockerfile`: also copy
  `darm/crates/` into the build context (they are workspace members now).
- `.github/`: upstream workflows, `CODEOWNERS`, `FUNDING.yml` and `auto_assign.yml`
  moved to `darm/upstream/mesh-llm-github/`. They release to crates.io,
  publish images, deploy to Fly.io and target upstream's runners and
  reviewers; running them from this repository would fail at best. The active
  workflow is `.github/workflows/darm-ci.yml`.
- `LICENSE`: this repository's GPL-3.0; mesh-llm's Apache-2.0 license is kept
  at `licenses/mesh-llm-Apache-2.0.txt` (see `NOTICE`).

## Syncing a newer mesh-llm

```bash
git clone https://github.com/bnreplah/mesh-llm /tmp/mesh-llm
REV=$(git -C /tmp/mesh-llm rev-parse HEAD)
git -C /tmp/mesh-llm archive "$REV" | tar -x -C .        # overwrite upstream files
mv LICENSE licenses/mesh-llm-Apache-2.0.txt && git checkout LICENSE
# Upstream workflows come back with the archive: move them out of the active path.
for f in .github/workflows/*.yml; do [ "$(basename "$f")" = darm-ci.yml ] || mv "$f" darm/upstream/mesh-llm-github/workflows/; done
mv .github/CODEOWNERS .github/FUNDING.yml .github/auto_assign.yml darm/upstream/mesh-llm-github/
# re-apply the local changes listed above (they are small), update REV here,
cargo test -p darm-mesh-plugin && cargo check -p mesh-llm-host-runtime
```
