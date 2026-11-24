import { test } from 'node:test';
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { toHex } from '../src/bytes.js';
import { Canonical } from '../src/canonical.js';
import { digestHex } from '../src/hash.js';
import { rootHex, verifyProof } from '../src/merkle.js';

export const vectors = JSON.parse(
  readFileSync(new URL('../../spec/test-vectors.json', import.meta.url), 'utf8'),
);
const enc = new TextEncoder();

test('canonical encoding matches the spec example', () => {
  const c = new Canonical('example');
  c.u16(258).str('hi').bool(true).optFixed32(null);
  assert.equal(toHex(c.finish()), vectors.canonical_example.bytes);
});

test('BLAKE3 derive-key hashing matches the protocol vector', () => {
  assert.equal(digestHex('example', enc.encode('abc')), vectors.hash_digest_abc);
});

test('chain id is the hash of the chain name', () => {
  assert.equal(digestHex('chain-id', enc.encode(vectors.chain.name)), vectors.chain.chain_id);
});

test('merkle roots and proofs match the vectors', () => {
  const leaves = vectors.merkle.leaves;
  assert.equal(rootHex(leaves), vectors.merkle.root);
  assert.equal(rootHex([]), vectors.merkle.empty_root);
  const proof = vectors.merkle.proof_index_2;
  assert.ok(verifyProof(leaves[2], proof, vectors.merkle.root));
  assert.ok(!verifyProof(leaves[1], proof, vectors.merkle.root));
});

test('canonical encoder rejects out-of-range integers', () => {
  const c = new Canonical('x');
  assert.throws(() => c.u8(256), RangeError);
  assert.throws(() => c.u32(-1), RangeError);
  assert.throws(() => c.u64(2n ** 64n), RangeError);
  assert.throws(() => c.fixed32(new Uint8Array(31)), RangeError);
});
