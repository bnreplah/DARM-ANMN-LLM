import { test } from 'node:test';
import assert from 'node:assert/strict';
import { ChainVerifier } from '../src/ledger.js';
import { Ledger } from '../src/wallet.js';
import { vectors } from './protocol.test.js';

const c = vectors.companion;

test('balances derived from verified blocks equal the ledger totals', async () => {
  const v = new ChainVerifier({ name: c.chain.name, genesisValidators: c.chain.genesis_validators });
  const wallet = new Ledger();
  for (const block of c.chain.blocks) {
    await v.verifyBlock(block);
    wallet.apply(block);
  }
  for (const [name, want] of Object.entries(c.expected)) {
    const got = wallet.summary(want.node);
    assert.equal(got.balance, BigInt(want.balance), `${name} balance`);
    assert.equal(got.granted, BigInt(want.granted), `${name} granted`);
    assert.equal(got.settled_in, BigInt(want.settled_in), `${name} settled_in`);
    assert.equal(got.settled_out, BigInt(want.settled_out), `${name} settled_out`);
    assert.equal(got.entries_authored, want.entries_authored, `${name} entries`);
  }
});

test('a lying gateway is caught', async () => {
  const wallet = new Ledger();
  for (const block of c.chain.blocks) wallet.apply(block);
  const d = wallet.summary(c.expected.device.node);
  assert.deepEqual(Ledger.mismatches(c.expected.device, d), []);
  const lie = { ...c.expected.device, balance: 9999 };
  assert.deepEqual(Ledger.mismatches(lie, d).map((m) => m.key), ['balance']);
  assert.equal(wallet.activityFor(c.expected.device.node).length, 2);
});
