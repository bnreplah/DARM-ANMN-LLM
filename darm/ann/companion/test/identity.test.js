import { test } from 'node:test';
import assert from 'node:assert/strict';
import { fromHex, toHex } from '../src/bytes.js';
import { createIdentity, identityFromSeed, isSmallOrder, sign, verify } from '../src/identity.js';
import { entryCanonical, entryId, verifyEntry } from '../src/entries.js';
import { vectors } from './protocol.test.js';

test('a seeded identity has the protocol vector node id and signs deterministically', async () => {
  const id = await identityFromSeed(new Uint8Array(32).fill(7));
  assert.equal(id.nodeId, vectors.node_id);
  const message = fromHex(vectors.ledger_entry.canonical);
  assert.equal(await sign(id, message), vectors.ledger_entry.entry.signature);
});

test('a fresh identity cannot be exported and verifies only its own signatures', async () => {
  const id = await createIdentity();
  await assert.rejects(() => globalThis.crypto.subtle.exportKey('pkcs8', id.privateKey));
  const msg = new TextEncoder().encode('hello');
  const sig = await sign(id, msg);
  assert.ok(await verify(id.nodeId, msg, sig));
  assert.ok(!(await verify(id.nodeId, new TextEncoder().encode('hellp'), sig)));
  const other = await createIdentity();
  assert.ok(!(await verify(other.nodeId, msg, sig)));
  assert.ok(!(await verify(id.nodeId, msg, sig.replace('ed25519:', 'ed448:'))));
  assert.ok(!(await verify(id.nodeId, msg, sig.slice(0, -2))));
});

test('small-order public keys are refused even though they decode', () => {
  const identityPoint = new Uint8Array(32);
  identityPoint[0] = 1;
  assert.ok(isSmallOrder(identityPoint));
  assert.ok(isSmallOrder(new Uint8Array(32)));
  const minusOne = fromHex('ecffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff7f');
  assert.ok(isSmallOrder(minusOne));
  const nonCanonicalOne = fromHex('eeffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff7f');
  assert.ok(isSmallOrder(nonCanonicalOne), 'y = p + 1 is the identity too');
  assert.ok(isSmallOrder(fromHex('26e8958fc2b227b045c3f489f2ef98f0d5dfac05d3c63339b13802886d53fc05')));
  assert.ok(isSmallOrder(fromHex('c7176a703d4dd84fba3c0b760d10670f2a2053fa2c39ccc64ec7fd7792ac037a')));
  assert.ok(!isSmallOrder(fromHex(vectors.node_id)));
});

test('entry ids and signatures match the Rust vectors for every vector kind', async () => {
  const sets = [
    vectors.ledger_entry,
    vectors.credits.credit_grant,
    vectors.credits.compute_settle,
    vectors.equivocation,
    vectors.validator_set_change,
    vectors.pq_key_bind,
  ];
  for (const set of sets) {
    const signed = set.entry;
    if (set.canonical) {
      assert.equal(toHex(entryCanonical(signed.entry)), set.canonical, signed.entry.body.type);
    }
    assert.equal(entryId(signed.entry), set.id, signed.entry.body.type);
    assert.equal(await verifyEntry(signed, signed.entry.chain), set.id);
  }
});

test('a tampered entry or another chain is rejected', async () => {
  const signed = structuredClone(vectors.credits.credit_grant.entry);
  signed.entry.body.amount += 1;
  await assert.rejects(() => verifyEntry(signed, signed.entry.chain), /signature/);
  const real = vectors.credits.credit_grant.entry;
  await assert.rejects(() => verifyEntry(real, '00'.repeat(32)), /another chain/);
});
