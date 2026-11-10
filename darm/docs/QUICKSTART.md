# DARM-ANN-LLM quickstart: standalone, bring-your-own model/token, resources

DARM-ANN-LLM is usable out of the box three ways, from a single laptop up to a
small distributed mesh:

1. **Standalone** — one node, your own hardware, no mesh required.
2. **Bring your own model (BYOM)** — a local model file, or any
   OpenAI-compatible server (Ollama, vLLM, LM Studio, …).
3. **Bring your own token (BYOK)** — a keyed upstream provider reached through
   a local OpenAI-compatible gateway that holds the key.

Everything below serves the OpenAI-compatible API on `http://localhost:9337/v1`
and the web console on `http://localhost:3131`. The `darm` virtual model (signed
distributed memory, pheromone routing, compute pool) is **on by default**; set
`MESH_LLM_DARM=off` to disable it.

---

## 1. Standalone

```bash
mesh-llm serve --auto          # detects local GPUs/CPU and serves
```

`--auto` picks up local accelerators; with none, DARM's compute pool still
contributes this machine's **CPU** (`DARM_POOL_DEVICES=cpu,gpu` by default), so
a CPU-only box is a first-class node. Talk to it:

```bash
curl http://localhost:9337/v1/chat/completions -H 'Content-Type: application/json' \
  -d '{"model":"darm","messages":[{"role":"user","content":"hello"}]}'
```

A standalone node is its own single-member validator set, so its memories
finalize locally with no peers. Add nodes later and point them at the same
`DARM_CHAIN_NAME` / `DARM_VALIDATORS` to form a mesh (see the three-node recipe
in [`../README.md`](../README.md#configuration)).

## 2. Bring your own model

**A local model file or package** — managed by Skippy; see
[`config-models.md`](../../mesh/website/src/docs/pages/config-models.md) and
[`choose-a-model.md`](../../mesh/website/src/docs/pages/choose-a-model.md). In
short, start the server against a GGUF or model package:

```bash
mesh-llm serve --gguf /path/to/model.gguf
```

**An OpenAI-compatible server you already run** (Ollama, vLLM, LM Studio, TGI,
SGLang, …) — install the endpoint adapter once and point it at the server's URL;
no second model download is needed. Full recipe (and the from-another-machine
variant) is in
[`external-model-endpoints.md`](../../mesh/website/src/docs/pages/external-model-endpoints.md):

```bash
mesh-llm plugins install openai-endpoint
```

```toml
# ~/.mesh-llm/config.toml
[runtime]
mode = "on_demand"

[[plugin]]
name = "openai-endpoint"
url = "http://127.0.0.1:11434/v1"   # e.g. Ollama; change only the URL per provider
```

Mesh then serves those models (and the `darm` model on top of them) at
`:9337/v1`. List what resolved with `curl http://localhost:9337/v1/models`.

## 3. Bring your own token

The stock `openai-endpoint` adapter reads a **URL only** — it injects no API key
or Authorization header (see the API-key FAQ in
[`external-model-endpoints.md`](../../mesh/website/src/docs/pages/external-model-endpoints.md)).
So to use a keyed cloud provider, run a local OpenAI-compatible **gateway** that
holds the token and exposes an unauthenticated loopback URL, then point
`openai-endpoint` at that gateway:

```text
your app ─► mesh-llm :9337/v1 ─► openai-endpoint ─► http://127.0.0.1:<gw>/v1 ─► keyed provider
                                                      (gateway holds OPENAI_API_KEY)
```

Any gateway that speaks the OpenAI API works (for example a local LiteLLM
proxy, or the provider's own local shim); keep it bound to loopback so the
unauthenticated hop never leaves the machine. Providers that need no key
(Ollama, a local vLLM) need no gateway — point the adapter straight at them.

## Seeing nodes, memory and GPU — allocated vs available

The `darm_resources` operation reports the pool's capacity, what is committed to
in-flight jobs, and this node's own contribution. It is projected as an MCP tool
and an HTTP endpoint:

```bash
curl -s http://localhost:9337/api/plugins/darm/tools/darm_resources \
  -H 'Content-Type: application/json' -d '{}' | jq
```

```jsonc
{
  "enabled": true,
  "available": {                 // live pool capacity (this node + peers)
    "nodes": 3, "mobile_nodes": 1,
    "cpu_devices": 3, "cpu_cores": 40,
    "gpu_devices": 2, "npu_devices": 0,
    "accelerator_memory_mb": 49152,
    "cpu_gflops": 1800, "accelerator_gflops": 164000
  },
  "allocated": {                 // committed to jobs in flight right now
    "jobs": 1, "busy_workers": 2,
    "cpu_shards": 8, "gpu_shards": 24, "npu_shards": 0
  },
  "local": {                     // what THIS node offers
    "form_factor": "server", "contributing": true, "blocked_by": [],
    "cpu_cores": 16, "accelerator_memory_mb": 24576, "gflops": 82000,
    "devices": [ /* per-device profile */ ]
  }
}
```

- **available** is pooled capacity gathered from signed, unexpired capability
  adverts; subtract **allocated** to gauge headroom before adding nodes.
- **local** is always populated, so a standalone node sees its own CPU/GPU even
  before any peer advert arrives.

Related views: `darm_status` (ledger, memory, swarm, pheromone routes and the
same pool/plan facts), `darm_compute_status` (pool + sessions) and the web
console at `:3131`.
