"use strict";
const test = require("node:test");
const assert = require("node:assert/strict");
const M = require("./model.js");

const id = (n) => n.toString(16).padStart(2, "0").repeat(32);
const URL_A = "https://archive.example";

function info(overrides = {}) {
  return {
    chain: "test",
    chain_id: id(1),
    head: id(2),
    height: 3,
    validators: [
      { node: id(10), weight: 1 },
      { node: id(11), weight: 1 },
      { node: id(12), weight: 1 },
      { node: id(13), weight: 1 },
    ],
    committed_memories: 5,
    writes_enabled: false,
    ...overrides,
  };
}

function block(height, proposer, attesters, authors) {
  return {
    header: { height, proposer },
    attestations: attesters.map((validator) => ({ validator, signature: "ed25519:00" })),
    entries: authors.map((author) => ({ entry: { author } })),
  };
}

test("a monitor with no sources shows only itself, as a read-only edge", () => {
  const model = M.createModel();
  M.ensureObserver(model);
  const s = M.summary(model);
  assert.equal(s.alone, true);
  assert.equal(s.meshNodes, 0);
  assert.deepEqual([...model.nodes.keys()], [M.OBSERVER_ID]);
});

test("an unreachable source still leaves the monitor alone", () => {
  const model = M.createModel();
  M.addSource(model, URL_A);
  M.markError(model, URL_A, "network error");
  assert.equal(M.summary(model).alone, true);
  assert.equal(model.nodes.get(M.sourceId(URL_A)).ok, false);
});

test("info and blocks become nodes, roles and links", () => {
  const model = M.createModel();
  M.addSource(model, URL_A);
  M.applyInfo(model, URL_A, info());
  const events = M.applyBlocks(model, URL_A, [
    block(1, id(10), [id(10), id(11), id(12)], [id(20)]),
    block(2, id(11), [id(11), id(12), id(13)], [id(20), id(21)]),
  ]);
  assert.equal(events.length, 2);
  const s = M.summary(model);
  assert.equal(s.alone, false);
  assert.equal(s.validators, 4);
  assert.equal(s.meshNodes, 6);
  assert.deepEqual(s.quorum, { total: 4, needed: 3 });
  assert.equal(model.nodes.get(id(10)).proposed, 1);
  assert.equal(model.nodes.get(id(12)).attested, 2);
  assert.equal(model.nodes.get(id(20)).authored, 2);
  assert.equal(model.nodes.get(id(20)).validator, false);
  assert.ok(model.links.has("attest:" + id(11) + ">" + id(10)));
  assert.ok(model.links.has("entry:" + id(21) + ">" + id(11)));
  // A proposer attesting its own block is not a link to itself.
  assert.ok(!model.links.has("attest:" + id(10) + ">" + id(10)));
});

test("a validator removed by governance stops being shown as one", () => {
  const model = M.createModel();
  M.addSource(model, URL_A);
  M.applyInfo(model, URL_A, info());
  assert.equal(M.summary(model).validators, 4);
  // The archive now reports a smaller set in force, with new weights.
  M.applyInfo(
    model,
    URL_A,
    info({ validators: [{ node: id(10), weight: 2 }, { node: id(11), weight: 1 }, { node: id(12), weight: 1 }] }),
  );
  const s = M.summary(model);
  assert.equal(s.validators, 3);
  assert.deepEqual(s.quorum, { total: 4, needed: 3 });
  assert.equal(model.nodes.get(id(13)).validator, false);
  assert.equal(model.nodes.get(id(13)).weight, 0);
  assert.equal(model.nodes.get(id(10)).weight, 2);
  // The removed node stays on screen as an ordinary mesh node.
  assert.equal(M.summary(model).meshNodes, 4);
});

test("blocks at or below the cursor are not counted twice", () => {
  const model = M.createModel();
  M.addSource(model, URL_A);
  M.applyInfo(model, URL_A, info());
  const page = [block(1, id(10), [id(11)], [])];
  assert.equal(M.applyBlocks(model, URL_A, page).length, 1);
  assert.equal(M.applyBlocks(model, URL_A, page).length, 0);
  assert.equal(model.nodes.get(id(10)).proposed, 1);
});

test("quorum is strictly more than two thirds of the weight", () => {
  const w = (list) => M.quorumWeight(list.map((weight) => ({ weight }))).needed;
  assert.equal(w([1, 1, 1]), 3);
  assert.equal(w([1, 1, 1, 1]), 3);
  assert.equal(w([1, 1, 1, 1, 1, 1]), 5);
  assert.equal(w([1, 1, 1, 1, 1, 1, 1]), 5);
  assert.equal(w([5, 1, 1]), 5);
});

test("malformed archive data is rejected, not displayed", () => {
  const fresh = () => {
    const model = M.createModel();
    M.addSource(model, URL_A);
    return model;
  };
  const bad = [
    null,
    "text",
    info({ height: -1 }),
    info({ height: 1.5 }),
    info({ height: "3" }),
    info({ chain_id: "<img src=x onerror=alert(1)>" }),
    info({ head: id(0xab).toUpperCase() }),
    info({ validators: "all" }),
    info({ validators: [{ node: "short", weight: 1 }] }),
    info({ validators: [{ node: id(10), weight: -5 }] }),
    info({ validators: new Array(257).fill({ node: id(10), weight: 1 }) }),
  ];
  for (const raw of bad) assert.throws(() => M.applyInfo(fresh(), URL_A, raw));

  const model = fresh();
  M.applyInfo(model, URL_A, info());
  const before = model.nodes.size;
  for (const page of [
    "nope",
    new Array(65).fill(block(1, id(10), [], [])),
    [{ header: { height: 1, proposer: "<script>" } }],
    [block(1, id(10), ["javascript:alert(1)"], [])],
    [block(1, id(10), [], ["x".repeat(64)])],
    [{ header: { height: 1, proposer: id(10) }, entries: new Array(513).fill({}) }],
  ]) {
    assert.throws(() => M.applyBlocks(model, URL_A, page));
  }
  assert.equal(model.nodes.size, before, "nothing from a rejected page is kept as a node");
  // A long chain name is truncated rather than trusted.
  assert.equal(M.applyInfo(fresh(), URL_A, info({ chain: "c".repeat(500) })).chain.length, 64);
});

test("node and source counts are capped", () => {
  const model = M.createModel();
  M.addSource(model, URL_A);
  M.applyInfo(model, URL_A, info({ height: 100000 }));
  model.sources.get(URL_A).cursor = 0;
  const wide = (n) => (0x100000 + n).toString(16).padStart(64, "0");
  for (let h = 1; h <= 40; h += 1) {
    const authors = Array.from({ length: 50 }, (_, i) => wide(h * 100 + i));
    M.applyBlocks(model, URL_A, [block(h, id(10), [], authors)]);
  }
  assert.ok(model.nodes.size <= M.MAX_NODES);
  for (let i = 0; i < 40; i += 1) M.addSource(model, "https://a" + i + ".example");
  assert.ok(model.sources.size <= 16);
});

test("only plain http(s) sources are accepted", () => {
  assert.equal(M.normalizeSource("https://a.example/"), "https://a.example");
  assert.equal(M.normalizeSource(" http://127.0.0.1:8088 "), "http://127.0.0.1:8088");
  assert.equal(M.normalizeSource("https://a.example/darm/"), "https://a.example/darm");
  for (const bad of [
    "javascript:alert(1)",
    "file:///etc/passwd",
    "data:text/html,x",
    "https://user:pw@a.example",
    "https://a.example/?q=1",
    "https://a.example/#x",
    "not a url",
    "",
    42,
    "https://a.example/" + "x".repeat(400),
  ]) {
    assert.equal(M.normalizeSource(bad), null, String(bad));
  }
  const model = M.createModel();
  assert.ok(M.addSource(model, URL_A));
  assert.equal(M.addSource(model, URL_A + "/"), null, "duplicates are ignored");
});

test("simulation is flagged and never looks like a real source", () => {
  const model = M.createModel();
  const sim = M.simulate(model, 24);
  assert.equal(model.simulated, true);
  assert.ok(sim.url.endsWith(".invalid"));
  let seed = 7;
  const random = () => ((seed = (seed * 16807) % 2147483647) / 2147483647);
  for (let h = 1; h <= 20; h += 1) M.simulateBlock(model, sim, h, random);
  const s = M.summary(model);
  assert.equal(s.height, 20);
  assert.equal(s.meshNodes, 24);
  assert.equal(s.chain, "simulated");
});
