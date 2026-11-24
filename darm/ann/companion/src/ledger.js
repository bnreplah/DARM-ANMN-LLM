// Light verification of the finalized ledger (spec §4.3, §4.6).
//
// For every block this checks what a validator quorum signed: the hash chain,
// the validator-set commitment in force, the proposer, the entries root, each
// entry's author signature, and the finality certificate (attestations, or
// precommits of a round on lock-based chains).
//
// It does NOT re-run the state rules (§4.4) such as "a grant needs a passed
// proposal" or "one registration per node". Those are what the quorum vouched
// for by signing the block, so a verifier that trusts less than two thirds of
// the validator weight must run a full node instead. The credit totals the
// wallet derives follow from the entries and are what the ledger itself
// computes (§4.7).

import { fromHex, toHex, utf8 } from './bytes.js';
import { Canonical } from './canonical.js';
import { verifyEntry } from './entries.js';
import { digest } from './hash.js';
import { verify } from './identity.js';
import { big, small } from './json.js';
import { rootHex } from './merkle.js';

export const MAX_ENTRIES_PER_BLOCK = 512;
export const MAX_CLOCK_SKEW_MS = 5 * 60 * 1000;

export class VerificationError extends Error {
  constructor(height, reason) {
    super(`block ${height}: ${reason}`);
    this.name = 'VerificationError';
    this.height = height;
    this.reason = reason;
  }
}

const byNode = (a, b) => (a.node < b.node ? -1 : a.node > b.node ? 1 : 0);

export function validatorCommitment(members) {
  const sorted = [...members].sort(byNode);
  const c = new Canonical('validator-set').u32(sorted.length);
  for (const m of sorted) c.fixed32(m.node).u64(m.weight);
  return toHex(digest('validator-set', c.finish()));
}

export const chainIdOf = (name) => toHex(digest('chain-id', utf8(name)));

export function genesisHash(chainId, members) {
  const c = new Canonical('genesis').fixed32(chainId).fixed32(validatorCommitment(members));
  return toHex(digest('genesis', c.finish()));
}

export function headerHash(h) {
  const c = new Canonical('block-header')
    .fixed32(h.chain)
    .u64(h.height)
    .u32(small(h.round))
    .fixed32(h.prev)
    .fixed32(h.entries_root)
    .u32(small(h.entry_count))
    .u64(h.timestamp_ms)
    .fixed32(h.proposer)
    .fixed32(h.validators);
  return toHex(digest('block', c.finish()));
}

const proposalMessage = (chain, hash) => new Canonical('block-proposal').fixed32(chain).fixed32(hash).finish();
const attestationMessage = (chain, height, hash) =>
  new Canonical('attestation').fixed32(chain).u64(height).fixed32(hash).finish();
const precommitMessage = (chain, height, round, hash) =>
  new Canonical('consensus-vote').fixed32(chain).u8(2).u64(height).u32(round).optFixed32(hash).finish();

function hasQuorum(weights, total) {
  let sum = 0n;
  for (const w of weights) sum += w;
  return 3n * sum > 2n * total;
}

export class ChainVerifier {
  /**
   * @param {{name: string, genesisValidators: {node: string, weight: number|string}[]}} config
   *   What the user pinned about the chain: its name and its genesis validators.
   */
  constructor({ name, genesisValidators }) {
    this.name = name;
    this.chainId = chainIdOf(name);
    this.genesis = genesisValidators.map((m) => ({ node: m.node, weight: big(m.weight) })).sort(byNode);
    this.genesisHash = genesisHash(
      this.chainId,
      this.genesis.map((m) => ({ node: m.node, weight: m.weight })),
    );
    this.height = 0;
    this.head = this.genesisHash;
    this.timestampMs = 0;
    /** Validator-set changes seen so far: {effective_height, members}. */
    this.changes = [];
  }

  /** The validator set that signs the block at `height`. */
  validatorsAt(height) {
    let members = this.genesis;
    let best = -1;
    for (const change of this.changes) {
      if (change.effective_height <= height && change.effective_height >= best) {
        members = change.members;
        best = change.effective_height;
      }
    }
    return members;
  }

  /** Everything needed to resume verification later. */
  checkpoint() {
    return {
      name: this.name,
      height: this.height,
      head: this.head,
      timestampMs: this.timestampMs,
      changes: this.changes.map((c) => ({
        effective_height: c.effective_height,
        members: c.members.map((m) => ({ node: m.node, weight: m.weight.toString() })),
      })),
    };
  }

  restore(cp) {
    if (cp.name !== this.name) throw new Error('checkpoint is for another chain');
    this.height = cp.height;
    this.head = cp.head;
    this.timestampMs = cp.timestampMs;
    this.changes = cp.changes.map((c) => ({
      effective_height: c.effective_height,
      members: c.members.map((m) => ({ node: m.node, weight: big(m.weight) })),
    }));
  }

  /** Verify the next block; on success the verifier advances past it. */
  async verifyBlock(block, { nowMs } = {}) {
    const header = block.header;
    const height = small(header.height);
    const fail = (reason) => {
      throw new VerificationError(height, reason);
    };
    if (header.chain !== this.chainId) fail('wrong chain');
    if (height !== this.height + 1) fail(`expected height ${this.height + 1}`);
    if (header.prev !== this.head) fail('does not link to the previous block');
    const members = this.validatorsAt(height);
    if (header.validators !== validatorCommitment(members)) fail('validator set does not match the one in force');
    if (header.timestamp_ms < this.timestampMs) fail('timestamp goes backwards');
    if (nowMs !== undefined && header.timestamp_ms > nowMs + MAX_CLOCK_SKEW_MS) fail('timestamp is in the future');

    const round = small(header.round);
    const expectedProposer = members[(height + round) % members.length].node;
    if (header.proposer !== expectedProposer) fail('not the designated proposer');
    const hash = headerHash(header);
    if (!(await verify(header.proposer, proposalMessage(this.chainId, hash), block.proposer_signature))) {
      fail('proposer signature does not verify');
    }

    const entries = block.entries;
    if (entries.length < 1 || entries.length > MAX_ENTRIES_PER_BLOCK) fail('entry count out of range');
    if (small(header.entry_count) !== entries.length) fail('entry_count mismatch');
    let ids;
    try {
      ids = await Promise.all(entries.map((e) => verifyEntry(e, this.chainId)));
    } catch (error) {
      fail(error.message);
    }
    if (new Set(ids).size !== ids.length) fail('duplicate entry in block');
    if (rootHex(ids) !== header.entries_root) fail('entries_root mismatch');
    for (const e of entries) {
      if (e.entry.timestamp_ms > header.timestamp_ms + MAX_CLOCK_SKEW_MS) fail('entry is newer than its block');
    }

    await this.#checkCertificate(block, hash, members, fail);

    this.height = height;
    this.head = hash;
    this.timestampMs = header.timestamp_ms;
    for (const e of entries) {
      if (e.entry.body.type === 'validator_set_change') {
        this.changes.push({
          effective_height: small(e.entry.body.effective_height),
          members: e.entry.body.members.map((m) => ({ node: m.node, weight: big(m.weight) })).sort(byNode),
        });
      }
    }
    return { height, hash, ids };
  }

  async #checkCertificate(block, hash, members, fail) {
    const header = block.header;
    const lockBased = block.commit_round !== undefined && block.commit_round !== null;
    const commitRound = lockBased ? small(block.commit_round) : 0;
    if (lockBased && commitRound < small(header.round)) fail('commit_round is below the block round');
    const weight = new Map(members.map((m) => [m.node, m.weight]));
    const total = members.reduce((sum, m) => sum + m.weight, 0n);
    if (block.attestations.length > members.length) fail('more attestations than validators');
    const message = lockBased
      ? precommitMessage(this.chainId, small(header.height), commitRound, hash)
      : attestationMessage(this.chainId, small(header.height), hash);
    const seen = new Set();
    const counted = [];
    for (const a of block.attestations) {
      if (seen.has(a.validator)) fail('duplicate attestation');
      seen.add(a.validator);
      if (!weight.has(a.validator)) fail('attestation from a non-validator');
      if (!(await verify(a.validator, message, a.signature))) fail('attestation signature does not verify');
      counted.push(weight.get(a.validator));
    }
    if (!hasQuorum(counted, total)) fail('not enough validator weight attested');
  }
}

export { fromHex };
