import { test } from 'node:test';
import assert from 'node:assert/strict';
import { execFileSync } from 'node:child_process';
import { writeFileSync, mkdtempSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { normalizeArchive, validateInfo, envFile, joinScript } from './kit.js';

const info = {
  chain: 'darm-demo', genesis: 'a'.repeat(64), height: 12,
  genesis_validators: [{ node: 'b'.repeat(64), weight: 1 }, { node: 'c'.repeat(64), weight: 1 }],
  activations: { credits: 1200, 'pq-identity': 1200 },
};
const choices = { formFactor: 'laptop', workloads: ['inference', 'verification'], sharePct: 50, requireCharging: true };

test('archive URLs: https only, http for localhost, no credentials', () => {
  assert.equal(normalizeArchive(' https://a.example.org/ '), 'https://a.example.org');
  assert.equal(normalizeArchive('http://127.0.0.1:8088'), 'http://127.0.0.1:8088');
  for (const bad of ['http://a.example.org', 'https://u:p@a.example.org', 'https://a.example.org/?x=1', 'javascript:alert(1)', 'nope'])
    assert.throws(() => normalizeArchive(bad), bad);
});

test('hostile or malformed /v1/info is rejected before it can reach a file', () => {
  assert.throws(() => validateInfo({ ...info, chain: "x'; rm -rf ~; '" }));
  assert.throws(() => validateInfo({ ...info, genesis: 'zz' }));
  assert.throws(() => validateInfo({ ...info, genesis_validators: [{ node: "'$(id)'" }] }));
  assert.throws(() => validateInfo({ ...info, activations: { 'a;b': 1 } }));
  assert.throws(() => validateInfo({ ...info, genesis_validators: [] }));
});

test('env file carries the cluster settings and parses as shell', () => {
  const net = validateInfo(info);
  const env = envFile(net, choices, 'https://a.example.org');
  assert.match(env, /DARM_CHAIN_NAME='darm-demo'/);
  assert.match(env, /DARM_ACTIVATE='credits@1200,pq-identity@1200'/);
  assert.match(env, /DARM_POOL_REQUIRE_CHARGING=1/);
  const dir = mkdtempSync(join(tmpdir(), 'kit-'));
  writeFileSync(join(dir, 'darm-join.env'), env);
  const out = execFileSync('bash', ['-c', `. ${dir}/darm-join.env && echo "$DARM_CHAIN_NAME|$DARM_VALIDATORS|$DARM_WORKLOADS"`]).toString().trim();
  assert.equal(out, `darm-demo|${'b'.repeat(64)},${'c'.repeat(64)}|inference,verification`);
});

test('bad device choices are rejected', () => {
  const net = validateInfo(info);
  for (const bad of [{ formFactor: 'toaster' }, { workloads: [] }, { workloads: ['mining'] }, { sharePct: 0 }, { sharePct: 101 }, { sharePct: 'x' }])
    assert.throws(() => envFile(net, { ...choices, ...bad }, 'https://a.example.org'));
});

test('join script is valid bash', () => {
  const dir = mkdtempSync(join(tmpdir(), 'kit-'));
  writeFileSync(join(dir, 'join.sh'), joinScript(validateInfo(info)));
  execFileSync('bash', ['-n', join(dir, 'join.sh')]);
});

import { PROFILES, guessFormFactor, isMobile, validateChoices } from './kit.js';
import { loadSettings, saveSettings, candidateArchives } from './settings.js';
import { connect } from './net.js';

test('profiles produce valid choices and device guesses are sane', () => {
  for (const p of Object.values(PROFILES)) {
    assert.doesNotThrow(() => validateChoices({ formFactor: 'laptop', workloads: ['inference'], sharePct: p.sharePct, requireCharging: p.requireCharging }));
  }
  assert.equal(guessFormFactor('Mozilla/5.0 (iPhone; CPU iPhone OS 17_0)'), 'phone');
  assert.equal(guessFormFactor('Mozilla/5.0 (Linux; Android 14; Pixel) Mobile'), 'phone');
  assert.equal(guessFormFactor('Mozilla/5.0 (iPad)'), 'tablet');
  assert.equal(guessFormFactor('Mozilla/5.0 (X11; Linux x86_64)'), 'laptop');
  assert.equal(isMobile('Mozilla/5.0 (iPhone)'), true);
});

const memStore = () => { const m = new Map(); return { getItem: (k) => m.get(k) ?? null, setItem: (k, v) => void m.set(k, v), removeItem: (k) => void m.delete(k) }; };

test('tokens stay in the session unless the user opts to remember them', () => {
  const local = memStore(), session = memStore();
  saveSettings({ archive: 'https://a.example.org', readToken: 'secret', remember: false }, local, session);
  assert.equal(local.getItem('darm.settings.v1'), null);
  assert.equal(loadSettings(local, session).readToken, 'secret');
  saveSettings({ archive: 'https://a.example.org', readToken: 'secret', remember: true }, local, session);
  assert.equal(session.getItem('darm.settings.v1'), null);
  assert.equal(loadSettings(local, session).remember, true);
  const bad = { getItem: () => '{not json', setItem() {}, removeItem() {} };
  assert.equal(loadSettings(bad, bad).archive, '');
});

test('the user archive goes first, then the site pivot list without duplicates', () => {
  assert.deepEqual(candidateArchives({ archive: 'https://mine.example' }, { nodes: ['https://p1.example', 'https://mine.example'], archive: 'https://p1.example' }),
    ['https://mine.example', 'https://p1.example']);
  assert.deepEqual(candidateArchives({ archive: '' }, {}), []);
});

test('connect fails over across the pivot list and reports why each failed', async () => {
  const good = { ok: true, status: 200, json: async () => info };
  const calls = [];
  const fetchFn = async (url, opts) => {
    calls.push([url, opts.headers.authorization]);
    if (url.startsWith('https://down.example')) throw new TypeError('network');
    if (url.startsWith('https://locked.example')) return { ok: false, status: 401 };
    return good;
  };
  const r = await connect(['https://down.example', 'https://locked.example', 'http://bad.example', 'https://up.example'], { readToken: 't', fetchFn });
  assert.equal(r.archive, 'https://up.example');
  assert.equal(r.tried, 4);
  assert.deepEqual(r.failures.map((f) => f.reason), ['network', 'needs a read token', 'The archive must use https (http only for localhost).']);
  assert.equal(calls[0][1], 'Bearer t');
  await assert.rejects(() => connect(['https://down.example'], { fetchFn }), (e) => e.failures.length === 1);
});

import { readRegistry, onChainArchives } from './chain.js';

const word = (n) => BigInt(n).toString(16).padStart(64, '0');
const strWord = (s) => { const h = Buffer.from(s).toString('hex'); return word(s.length) + h.padEnd(Math.ceil(h.length / 64) * 64, '0'); };
function reply(admin, genesis, urls) {
  const parts = urls.map(strWord);
  let off = urls.length * 32; const offsets = [];
  for (const p of parts) { offsets.push(word(off)); off += p.length / 2; }
  return '0x' + admin.replace('0x', '').padStart(64, '0') + genesis + word(1) + word(1700000000) + word(160) + word(urls.length) + offsets.join('') + parts.join('');
}
const ADMIN = '0x' + 'ab'.repeat(20);
const entry = { name: 'sepolia', rpc: 'https://rpc.example', registry: '0x' + 'cd'.repeat(20), chainId: '0x' + '11'.repeat(32), admin: ADMIN };
const rpc = (body) => async () => ({ ok: true, json: async () => ({ result: body }) });

test('on-chain list is used only when the registered admin and genesis match the pins', async () => {
  const good = reply(ADMIN, '22'.repeat(32), ['https://a.example.org']);
  assert.deepEqual((await readRegistry(entry, { fetchFn: rpc(good) })).urls, ['https://a.example.org']);
  // A squatter registered the same chain id first: wrong admin, ignored.
  assert.equal(await readRegistry(entry, { fetchFn: rpc(reply('0x' + 'ee'.repeat(20), '22'.repeat(32), ['https://evil.example'])) }), null);
  // Pinned genesis must match too.
  assert.equal(await readRegistry({ ...entry, genesis: '33'.repeat(32) }, { fetchFn: rpc(good) }), null);
  assert.ok(await readRegistry({ ...entry, genesis: '22'.repeat(32) }, { fetchFn: rpc(good) }));
});

test('malformed or hostile registry replies and configs are ignored, not trusted', async () => {
  for (const bad of ['0x', '0x1234', '0x' + 'ff'.repeat(300), reply(ADMIN, '22'.repeat(32), ['https://a']).replace(/.{64}$/, 'ff'.repeat(32))])
    assert.equal(await readRegistry(entry, { fetchFn: rpc(bad) }), null);
  assert.equal(await readRegistry({ ...entry, rpc: 'http://insecure.example' }), null);
  assert.equal(await readRegistry({ ...entry, admin: 'nope' }), null);
  assert.equal(await readRegistry(entry, { fetchFn: async () => { throw new TypeError('down'); } }), null);
});

test('an archive found on-chain must still match the registry genesis and use https', async () => {
  const fetchFn = async () => ({ ok: true, status: 200, json: async () => info });
  const match = await onChainArchives([entry], { fetchFn: rpc(reply(ADMIN, 'a'.repeat(64), ['http://plain.example', 'https://up.example'])) });
  const r = await connect(match, { fetchFn });
  assert.equal(r.archive, 'https://up.example');
  assert.equal(r.via, 'sepolia');
  assert.equal(r.failures[0].reason, 'The archive must use https (http only for localhost).');
  const wrong = await onChainArchives([entry], { fetchFn: rpc(reply(ADMIN, 'c'.repeat(64), ['https://up.example'])) });
  await assert.rejects(() => connect(wrong, { fetchFn }), (e) => e.failures[0].reason === 'genesis does not match the registry');
});
