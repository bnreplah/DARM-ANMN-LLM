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
