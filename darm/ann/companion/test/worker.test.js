import { test } from 'node:test';
import assert from 'node:assert/strict';
import { Companion } from '../src/worker.js';
import { identityFromSeed } from '../src/identity.js';
import { vectors } from './protocol.test.js';

const c = vectors.companion;
const memoryStore = () => {
  const m = new Map();
  return { get: async (k) => structuredClone(m.get(k)), set: async (k, v) => void m.set(k, structuredClone(v)) };
};

function archive(blocks, posted) {
  const info = {
    chain: c.chain.name, chain_id: c.chain.chain_id, genesis: c.chain.blocks[0].header.prev,
    genesis_validators: c.chain.genesis_validators, height: blocks.length, head: c.chain.head,
  };
  const reply = (status, body) => ({ ok: status < 300, status, text: async () => JSON.stringify(body) });
  return async (url, opts = {}) => {
    const u = new URL(url);
    if (u.pathname === '/v1/info') return reply(200, info);
    if (u.pathname === '/v1/ledger/blocks') {
      const after = Number(u.searchParams.get('after'));
      return reply(200, blocks.slice(after, after + Number(u.searchParams.get('limit'))));
    }
    if (u.pathname.endsWith('/receipts') && opts.method === 'POST') {
      posted.push({ headers: opts.headers, body: JSON.parse(opts.body) });
      return reply(201, JSON.parse(opts.body));
    }
    if (u.pathname.endsWith('/wallet')) return reply(404, 'no');
    return reply(404, 'no');
  };
}

async function setup(blocks = c.chain.blocks) {
  const posted = [];
  const identity = await identityFromSeed(new Uint8Array(32).fill(7));
  const app = new Companion({ identity, store: memoryStore(), fetch: archive(blocks, posted), now: () => 1700000100000 });
  const found = await app.discover('https://archive.example/');
  await app.pin(found);
  return { app, posted };
}

test('discover, pin, verify the chain and submit a signed audit receipt', async () => {
  const { app, posted } = await setup();
  const r = await app.sync();
  assert.equal(r.to, 12);
  assert.equal(r.invalid, null);
  assert.equal(await app.submitAudit(r) !== null, true);
  assert.equal(posted.length, 1);
  assert.equal(posted[0].body.verdict, 'ok');
  assert.equal(posted[0].body.from_height, 1);
  assert.equal(posted[0].body.to_height, 12);
  assert.equal(posted[0].headers['x-darm-node'], app.nodeId);
  assert.equal(app.wallet.summary(c.expected.device.node).balance, 145n);
});

test('an archive serving a forged block is stopped and reported invalid', async () => {
  const blocks = structuredClone(c.chain.blocks);
  blocks[5].entries[0].entry.body.amount = 999999;
  const { app, posted } = await setup(blocks);
  const r = await app.sync();
  assert.equal(r.to, 5);
  assert.equal(r.invalid.height, 6);
  await app.submitAudit(r);
  assert.equal(posted[0].body.verdict, 'invalid');
  assert.equal(posted[0].body.to_height, 5);
  assert.equal(app.wallet.balance(c.expected.coordinator.node), 500n, 'forged block never reaches the wallet');
});

test('a second sync with nothing new produces no receipt', async () => {
  const { app } = await setup();
  await app.sync();
  assert.equal(await app.submitAudit(await app.sync()), null);
});

test('verified state survives a restart and a different chain cannot be pinned', async () => {
  const { app } = await setup();
  await app.sync();
  const again = new Companion({ identity: app.identity, store: app.store, fetch: app.http, now: app.now });
  assert.equal(await again.resume(), true);
  assert.equal(again.verifier.height, 12);
  await assert.rejects(() => again.pin({ chain: 'x', genesis: '00'.repeat(32), validators: [] }), /different chain/);
});

test('an archive whose info disagrees with its own validators is refused', async () => {
  const lying = async (url) => ({ ok: true, status: 200, text: async () => JSON.stringify({
    chain: c.chain.name, chain_id: c.chain.chain_id, genesis: '00'.repeat(32), genesis_validators: c.chain.genesis_validators, height: 0 }) });
  const app = new Companion({ identity: await identityFromSeed(new Uint8Array(32).fill(7)), store: memoryStore(), fetch: lying });
  await assert.rejects(() => app.discover('https://x.example'), /do not produce/);
});
