import { test } from 'node:test';
import assert from 'node:assert/strict';
import { ChainVerifier, VerificationError, chainIdOf, genesisHash, headerHash } from '../src/ledger.js';
import { vectors } from './protocol.test.js';
import { parseJson } from '../src/json.js';

const c = vectors.companion;
const fresh = () =>
  new ChainVerifier({ name: c.chain.name, genesisValidators: c.chain.genesis_validators });
const clone = (v) => structuredClone(v);

test('the pinned chain derives the vector chain id and genesis', () => {
  const v = fresh();
  assert.equal(v.chainId, c.chain.chain_id);
  assert.equal(v.genesisHash, c.chain.blocks[0].header.prev);
  assert.equal(chainIdOf(vectors.chain.name), vectors.chain.chain_id);
  assert.equal(
    genesisHash(vectors.chain.chain_id, [{ node: vectors.node_id, weight: 1 }]),
    vectors.chain.genesis_hash,
  );
});

test('block hashes match the Rust vectors', () => {
  assert.equal(headerHash(vectors.block.block.header), vectors.block.hash);
});

test('the whole companion chain verifies, across a validator-set change', async () => {
  const v = fresh();
  for (const block of c.chain.blocks) await v.verifyBlock(block);
  assert.equal(v.height, c.chain.height);
  assert.equal(v.head, c.chain.head);
  assert.equal(v.changes.length, 1);
  assert.equal(c.chain.blocks[11].attestations.length, 3, 'height 12 needs all three validators');
});

test('verification can resume from a checkpoint', async () => {
  const a = fresh();
  for (const block of c.chain.blocks.slice(0, 8)) await a.verifyBlock(block);
  const cp = JSON.parse(JSON.stringify(a.checkpoint()));
  const b = fresh();
  b.restore(cp);
  for (const block of c.chain.blocks.slice(8)) await b.verifyBlock(block);
  assert.equal(b.head, c.chain.head);
  assert.throws(() => new ChainVerifier({ name: 'other', genesisValidators: c.chain.genesis_validators }).restore(cp));
});

async function rejects(mutate, pattern, upTo = 0) {
  const v = fresh();
  const blocks = clone(c.chain.blocks);
  for (const block of blocks.slice(0, upTo)) await v.verifyBlock(block);
  const target = blocks[upTo];
  mutate(target, blocks);
  await assert.rejects(() => v.verifyBlock(target), (e) => e instanceof VerificationError && pattern.test(e.reason), pattern.source);
}

test('forged or altered blocks are rejected for the right reason', async () => {
  await rejects((b) => { b.entries[0].entry.body.role = 'x'; }, /signature|entries_root/);
  await rejects((b) => { b.header.height = 9; }, /expected height/);
  await rejects((b) => { b.header.prev = '00'.repeat(32); }, /link/);
  await rejects((b) => { b.header.timestamp_ms -= 1; }, /proposer signature|backwards/);
  await rejects((b) => { b.attestations.pop(); }, /not enough validator weight/);
  await rejects((b) => { b.attestations.push(b.attestations[0]); }, /more attestations|duplicate/);
  await rejects((b) => { b.proposer_signature = b.attestations[0].signature; }, /proposer signature/);
  await rejects((b) => { b.header.validators = '11'.repeat(32); }, /validator set/);
  await rejects((b) => { b.header.round = 1; }, /proposer/);
  await rejects((b) => { b.entries.push(b.entries[0]); b.header.entry_count = 4; }, /proposer signature|duplicate/);
  await rejects((b) => { b.header.entry_count = 7; }, /proposer signature|entry_count/);
  await rejects((b) => { b.header.chain = '22'.repeat(32); }, /wrong chain/);
});

test('a validator set that changes at height 12 cannot be applied early or ignored', async () => {
  // Height 12 verified without learning of the change (attestations from only
  // two validators) must fail the quorum.
  const v = fresh();
  for (const block of c.chain.blocks.slice(0, 11)) await v.verifyBlock(block);
  const twelve = clone(c.chain.blocks[11]);
  twelve.attestations.pop();
  await assert.rejects(() => v.verifyBlock(twelve), /not enough validator weight/);
});

test('a lock-based certificate verifies, and not for another round', async () => {
  const locking = vectors.lock_based_finality.block_committed_in_round_3;
  const v = new ChainVerifier({
    name: vectors.chain.name,
    genesisValidators: [{ node: vectors.node_id, weight: 1 }],
  });
  const bad = clone(locking);
  bad.commit_round = 2;
  await assert.rejects(() => v.verifyBlock(bad), /attestation signature/);
  await v.verifyBlock(locking);
  assert.equal(v.height, 1);
});

test('64-bit nonces survive parsing so signatures still verify', async () => {
  // Entry nonces are random u64s. Round-tripping through JSON.parse would
  // round them; parseJson keeps the digits exact.
  const text = '{"nonce":18446744073709551557,"list":[9007199254740993,5],"n":12}';
  const parsed = parseJson(text);
  assert.equal(parsed.nonce, '18446744073709551557');
  assert.equal(parsed.list[0], '9007199254740993');
  assert.equal(parsed.n, 12);
  const { Canonical } = await import('../src/canonical.js');
  assert.doesNotThrow(() => new Canonical('x').u64(parsed.nonce));
  assert.equal(JSON.parse(text).nonce === 18446744073709551557, true, 'plain JSON.parse would lose precision');
  assert.notEqual(BigInt(JSON.parse(text).nonce), 18446744073709551557n);
});
