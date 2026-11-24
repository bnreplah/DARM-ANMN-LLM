# DARM-ANN node monitor

A static page that shows a DARM-ANN network as a live graph: validators,
proposers and memory authors as nodes, with pulses travelling along the
links as blocks finalize. No build step, no dependencies, four files.

It is a **read-only edge**. It holds no key, signs nothing and never sends a
write; it only makes `GET` requests to the archive nodes you point it at.

## What it shows

| On screen | Where it comes from |
|---|---|
| Archive nodes (amber) | The archive URLs you configure. Red when unreachable. |
| Validators (cyan), weight, quorum | `GET /v1/info` |
| Other nodes (blue) | Authors of ledger entries in recent blocks |
| Links and pulses (magenta) | Attestations (validator to proposer) and entries (author to proposer) from `GET /v1/ledger/blocks` |
| This monitor (white ring) | The page itself, linked to each archive it reads |

When no archive is configured or reachable, the monitor is the only node on
screen and says so. "Preview with simulated nodes" (or `?demo=1`) draws a
synthetic network so you can see the visual; it is labelled as simulated and
uses ids that cannot be mistaken for real ones.

The node list is built from the ledger, so a node appears once it is a
validator or has authored, proposed or attested something in the recent
blocks the monitor has read. Nodes that only gossip are not visible to an
archive and therefore not shown.

## Deploy to GitHub Pages

1. In the repository: **Settings -> Pages -> Build and deployment -> Source:
   GitHub Actions**. (Pages on a private repository needs a paid GitHub plan;
   on a free plan, make the repository public or copy `monitor/` and the
   workflow into a public one.)
2. Optional: add a repository variable `DARM_MONITOR_NODES` with a
   comma-separated list of archive URLs. The page opens already pointed at
   them.
3. Run the **Node monitor (GitHub Pages)** workflow, or push a change under
   `monitor/`. The site is published at `https://<owner>.github.io/<repo>/`.
4. On each archive node, allow that origin (origin only, no path):

   ```bash
   DARM_NODE_CORS_ORIGINS=https://<owner>.github.io darm-node serve ...
   ```

Archives can also be added in the page, or by link:
`https://<owner>.github.io/<repo>/?node=https://archive.example`.

A page served over https can only read https archives (browsers block mixed
content), with `http://localhost` and `http://127.0.0.1` as the exception
for local testing.

## Run it locally

```bash
python3 -m http.server 8090 --bind 127.0.0.1 --directory monitor
DARM_NODE_CORS_ORIGINS=http://127.0.0.1:8090 darm-node serve --chain-name demo --validators <ids>
# open http://127.0.0.1:8090/?node=http://127.0.0.1:8088
```

## Security notes

- **Trust.** The monitor does not verify signatures. The archive node does
  that before serving anything, so the picture is as trustworthy as the
  archives you choose. A malicious archive can show a false picture; it
  cannot make the page do anything else.
- **Untrusted input.** Everything an archive returns is validated in
  `model.js` before use: ids must be 64 hex characters, counts must be
  bounded integers, lists are capped (400 nodes, 64 blocks per poll, 4 MiB
  per response) and a malformed block is rejected whole. Text reaches the
  page only through `textContent`; nothing is ever inserted as HTML.
- **Content Security Policy.** No inline or third-party scripts or styles;
  network access is limited to https and localhost.
- **Requests.** `GET` only, no cookies or credentials, no redirects followed,
  no referrer. A read token, if you type one, is kept in memory for the
  session and sent only to the archive it was entered for. Do not publish a
  read token in `DARM_MONITOR_NODES` or a link.
- **Archive side.** `DARM_NODE_CORS_ORIGINS` takes exact origins, allows
  `GET` only and never credentials, so enabling the monitor does not open a
  way to write to the archive from a browser.
- **Privacy of the network.** Publishing a monitor for an archive with
  public reads shows node ids, validator weights and activity to anyone with
  the link. For a private mesh, gate reads with `--read-token-file` and keep
  the monitor local.

## Development

```bash
node --test monitor/model.test.js
```

`model.js` is the data model (pure, tested); `monitor.js` does polling and
drawing; `index.html` and `monitor.css` are the page.
