// DARM-ANN node monitor: data model.
//
// Pure functions that turn what an archive node serves (`/v1/info`,
// `/v1/ledger/blocks`) into a graph of nodes and links. No DOM, no network,
// so the same file runs in the browser and under `node --test`.
//
// Everything that arrives from an archive is untrusted input to this page:
// ids must be 64 hex characters, numbers must be finite and in range, and
// every collection is capped. Anything else is dropped, never displayed.
(function (root, factory) {
  const api = factory();
  if (typeof module === "object" && module.exports) module.exports = api;
  else root.DarmModel = api;
})(typeof self !== "undefined" ? self : globalThis, function () {
  "use strict";

  const MAX_NODES = 400;
  const MAX_LINKS = 1600;
  const MAX_BLOCKS_PER_POLL = 64;
  const MAX_ENTRIES_PER_BLOCK = 512;
  const MAX_VALIDATORS = 256;
  const HEX64 = /^[0-9a-f]{64}$/;
  const OBSERVER_ID = "observer";

  function isId(value) {
    return typeof value === "string" && HEX64.test(value);
  }

  function count(value, max) {
    return Number.isSafeInteger(value) && value >= 0 && value <= max ? value : null;
  }

  /// Accept only http(s) URLs without credentials; return a normalized
  /// origin + path with no trailing slash, or null.
  function normalizeSource(text) {
    if (typeof text !== "string" || text.length > 300) return null;
    let url;
    try {
      url = new URL(text.trim());
    } catch (_) {
      return null;
    }
    if (url.protocol !== "https:" && url.protocol !== "http:") return null;
    if (url.username || url.password || url.search || url.hash) return null;
    return (url.origin + url.pathname).replace(/\/+$/, "");
  }

  function createModel() {
    return {
      nodes: new Map(),
      links: new Map(),
      sources: new Map(),
      events: [],
      simulated: false,
    };
  }

  function touchNode(model, id, kind) {
    let node = model.nodes.get(id);
    if (!node) {
      if (model.nodes.size >= MAX_NODES) return null;
      node = {
        id,
        kind,
        validator: false,
        weight: 0,
        proposed: 0,
        attested: 0,
        authored: 0,
        lastHeight: 0,
        label: "",
        ok: true,
      };
      model.nodes.set(id, node);
    }
    return node;
  }

  function touchLink(model, from, to, kind) {
    if (from === to) return null;
    const key = kind + ":" + from + ">" + to;
    let link = model.links.get(key);
    if (!link) {
      if (model.links.size >= MAX_LINKS) return null;
      link = { key, from, to, kind, hits: 0 };
      model.links.set(key, link);
    }
    link.hits += 1;
    return link;
  }

  /// The monitor itself: a read-only edge node. It holds no key, signs
  /// nothing and cannot write; it only reads what archives serve.
  function ensureObserver(model) {
    const node = touchNode(model, OBSERVER_ID, "observer");
    node.label = "this monitor";
    return node;
  }

  function sourceId(url) {
    return "archive:" + url;
  }

  function addSource(model, url) {
    const normalized = normalizeSource(url);
    if (!normalized || model.sources.has(normalized) || model.sources.size >= 16) return null;
    const source = { url: normalized, height: 0, cursor: null, info: null, error: null, ok: false };
    model.sources.set(normalized, source);
    const node = touchNode(model, sourceId(normalized), "archive");
    if (node) {
      node.label = normalized.replace(/^https?:\/\//, "");
      node.ok = false;
    }
    ensureObserver(model);
    touchLink(model, OBSERVER_ID, sourceId(normalized), "read");
    return source;
  }

  /// Validate and apply `/v1/info`. Returns the cleaned summary or throws.
  function applyInfo(model, url, raw) {
    const source = model.sources.get(url);
    if (!source) throw new Error("unknown source");
    if (!raw || typeof raw !== "object") throw new Error("info is not an object");
    const height = count(raw.height, Number.MAX_SAFE_INTEGER);
    if (height === null) throw new Error("info.height is not a count");
    if (!isId(raw.chain_id) || !isId(raw.head)) throw new Error("info ids are malformed");
    if (!Array.isArray(raw.validators) || raw.validators.length > MAX_VALIDATORS) {
      throw new Error("info.validators is not a bounded list");
    }
    const validators = [];
    for (const item of raw.validators) {
      const weight = item ? count(item.weight, Number.MAX_SAFE_INTEGER) : null;
      if (!item || !isId(item.node) || weight === null) throw new Error("malformed validator");
      validators.push({ node: item.node, weight });
    }
    const info = {
      chain: typeof raw.chain === "string" ? raw.chain.slice(0, 64) : "",
      chainId: raw.chain_id,
      head: raw.head,
      height,
      validators,
      committedMemories: count(raw.committed_memories, Number.MAX_SAFE_INTEGER) ?? 0,
      writesEnabled: raw.writes_enabled === true,
    };
    source.info = info;
    source.height = height;
    source.ok = true;
    source.error = null;
    if (source.cursor === null) source.cursor = Math.max(0, height - 32);
    const archive = model.nodes.get(sourceId(url));
    if (archive) archive.ok = true;
    for (const validator of validators) {
      const node = touchNode(model, validator.node, "mesh");
      if (!node) continue;
      touchLink(model, sourceId(url), validator.node, "ledger");
    }
    refreshValidators(model);
    return info;
  }

  /// Validate and apply a page of blocks. Malformed blocks stop the page:
  /// an archive that serves one cannot be trusted for the rest.
  function applyBlocks(model, url, raw) {
    const source = model.sources.get(url);
    if (!source) throw new Error("unknown source");
    if (!Array.isArray(raw) || raw.length > MAX_BLOCKS_PER_POLL) {
      throw new Error("blocks is not a bounded list");
    }
    const applied = [];
    for (const block of raw) {
      const header = block && block.header;
      const height = header ? count(header.height, Number.MAX_SAFE_INTEGER) : null;
      if (height === null || !isId(header.proposer)) throw new Error("malformed block header");
      if (height <= source.cursor) continue;
      const entries = Array.isArray(block.entries) ? block.entries : [];
      const attestations = Array.isArray(block.attestations) ? block.attestations : [];
      if (entries.length > MAX_ENTRIES_PER_BLOCK || attestations.length > MAX_VALIDATORS) {
        throw new Error("block exceeds limits");
      }
      // Validate the whole block before any of it reaches the model.
      const attesters = attestations.map((a) => a && a.validator);
      const authors = entries.map((e) => e && e.entry && e.entry.author);
      if (!attesters.every(isId)) throw new Error("malformed attestation");
      if (!authors.every(isId)) throw new Error("malformed entry");

      const proposer = touchNode(model, header.proposer, "mesh");
      const event = { height, proposer: header.proposer, pulses: [] };
      if (proposer) {
        proposer.proposed += 1;
        proposer.lastHeight = Math.max(proposer.lastHeight, height);
      }
      for (const validator of attesters) {
        const node = touchNode(model, validator, "mesh");
        if (!node) continue;
        node.attested += 1;
        node.lastHeight = Math.max(node.lastHeight, height);
        const link = touchLink(model, validator, header.proposer, "attest");
        if (link) event.pulses.push(link.key);
      }
      for (const author of authors) {
        const node = touchNode(model, author, "mesh");
        if (!node) continue;
        node.authored += 1;
        node.lastHeight = Math.max(node.lastHeight, height);
        const link = touchLink(model, author, header.proposer, "entry");
        if (link) event.pulses.push(link.key);
      }
      source.cursor = height;
      applied.push(event);
    }
    model.events.push(...applied);
    if (model.events.length > 200) model.events.splice(0, model.events.length - 200);
    return applied;
  }

  /// A node is a validator when some source's latest info lists it. The
  /// set can shrink (validator-set governance), so flags are recomputed
  /// rather than only ever set.
  function refreshValidators(model) {
    const weights = new Map();
    for (const source of model.sources.values()) {
      if (!source.info) continue;
      for (const validator of source.info.validators) {
        weights.set(validator.node, validator.weight);
      }
    }
    for (const node of model.nodes.values()) {
      if (node.kind !== "mesh") continue;
      node.validator = weights.has(node.id);
      node.weight = node.validator ? weights.get(node.id) : 0;
    }
  }

  function markError(model, url, message) {
    const source = model.sources.get(url);
    if (!source) return;
    source.ok = false;
    source.error = String(message).slice(0, 200);
    const archive = model.nodes.get(sourceId(url));
    if (archive) archive.ok = false;
  }

  /// Weight needed for finality: strictly more than two thirds.
  function quorumWeight(validators) {
    const total = validators.reduce((sum, v) => sum + v.weight, 0);
    return { total, needed: Math.floor((total * 2) / 3) + 1 };
  }

  function summary(model) {
    const mesh = [...model.nodes.values()].filter((n) => n.kind === "mesh");
    const sources = [...model.sources.values()];
    const live = sources.filter((s) => s.ok);
    const best = live.reduce((top, s) => (!top || s.height > top.height ? s : top), null);
    return {
      meshNodes: mesh.length,
      validators: mesh.filter((n) => n.validator).length,
      sources: sources.length,
      reachable: live.length,
      height: best ? best.height : 0,
      chain: best && best.info ? best.info.chain : "",
      head: best && best.info ? best.info.head : "",
      quorum: best && best.info ? quorumWeight(best.info.validators) : null,
      // With nothing to read, the monitor is the only node on screen.
      alone: live.length === 0,
    };
  }

  /// Fill a model with clearly synthetic nodes for previewing the visual.
  /// Ids are derived from a counter, never from real data, and the model is
  /// flagged so the page can label everything as simulated.
  function simulate(model, size) {
    model.simulated = true;
    ensureObserver(model);
    const fake = (i) => i.toString(16).padStart(4, "0").repeat(16);
    const url = "https://simulated.invalid";
    model.sources.set(url, { url, height: 0, cursor: 0, info: null, error: null, ok: true });
    const archive = touchNode(model, sourceId(url), "archive");
    archive.label = "simulated archive";
    touchLink(model, OBSERVER_ID, sourceId(url), "read");
    const validators = [];
    const total = Math.max(4, Math.min(size, 60));
    for (let i = 1; i <= total; i += 1) {
      if (i <= Math.max(4, Math.floor(total / 3))) validators.push({ node: fake(i), weight: 1 });
      else touchNode(model, fake(i), "mesh");
    }
    applyInfo(model, url, {
      chain: "simulated",
      chain_id: fake(0xaaaa),
      head: fake(0xbbbb),
      height: 0,
      validators,
      committed_memories: 0,
      writes_enabled: false,
    });
    return { url, fake, total, validators };
  }

  function simulateBlock(model, sim, height, random) {
    const pick = (list) => list[Math.floor(random() * list.length)];
    const proposer = pick(sim.validators).node;
    const attestations = sim.validators
      .filter(() => random() < 0.85)
      .map((v) => ({ validator: v.node }));
    const entries = [];
    for (let i = 0; i < 1 + Math.floor(random() * 4); i += 1) {
      entries.push({ entry: { author: sim.fake(1 + Math.floor(random() * sim.total)) } });
    }
    const source = model.sources.get(sim.url);
    source.height = height;
    source.info.height = height;
    return applyBlocks(model, sim.url, [{ header: { height, proposer }, attestations, entries }]);
  }

  return {
    MAX_NODES,
    MAX_BLOCKS_PER_POLL,
    OBSERVER_ID,
    isId,
    normalizeSource,
    createModel,
    ensureObserver,
    addSource,
    applyInfo,
    applyBlocks,
    markError,
    quorumWeight,
    summary,
    simulate,
    simulateBlock,
    sourceId,
  };
});
