// Ledger entries (spec §4.2): canonical encoding of all twelve bodies, ids,
// and signature checks.

import { fromHex, toHex } from './bytes.js';
import { Canonical } from './canonical.js';
import { digest } from './hash.js';
import { verify } from './identity.js';
import { big, small } from './json.js';

const VOTE_KIND = { prevote: 1, precommit: 2 };

function encodeVote(c, vote) {
  c.u8(VOTE_KIND[vote.kind] ?? (() => { throw new RangeError('vote kind'); })())
    .u64(vote.height)
    .u32(small(vote.round))
    .optFixed32(vote.block)
    .fixed32(vote.validator)
    .bytes(fromHex(vote.signature.replace(/^ed25519:/, ''), 64));
}

function encodeMembers(c, members, effectiveHeight) {
  c.u64(effectiveHeight).u32(members.length);
  for (const m of members) c.fixed32(m.node).u64(m.weight);
}

const BODY = {
  node_registration: (c, b) =>
    c.u8(1).str(b.role).fixed32(b.genome).u32(small(b.generation)).optFixed32(b.parent ?? null),
  memory_commit: (c, b) => c.u8(2).fixed32(b.memory).u16(small(b.importance_milli)),
  memory_revoke: (c, b) => c.u8(3).fixed32(b.memory),
  reputation_report: (c, b) => c.u8(4).fixed32(b.subject).i32(small(b.delta_milli)).fixed32(b.evidence),
  proposal: (c, b) => c.u8(5).str(b.kind).fixed32(b.payload).u64(b.deadline_ms),
  vote: (c, b) => c.u8(6).fixed32(b.proposal).bool(b.support),
  compute_commit: (c, b) =>
    c.u8(7).fixed32(b.job).fixed32(b.result_root).fixed32(b.receipts_root).u32(small(b.shards)).u16(small(b.workers)),
  credit_grant: (c, b) => c.u8(8).fixed32(b.proposal).fixed32(b.to).u64(b.amount),
  compute_settle: (c, b) => {
    c.u8(9).fixed32(b.job).fixed32(b.receipts_root).u32(b.payments.length);
    for (const p of b.payments) c.fixed32(p.worker).u64(p.amount);
  },
  equivocation: (c, b) => {
    c.u8(10);
    encodeVote(c, b.first);
    encodeVote(c, b.second);
  },
  validator_set_change: (c, b) => {
    c.u8(11).fixed32(b.proposal);
    encodeMembers(c, b.members, b.effective_height);
  },
  pq_key_bind: (c, b) => c.u8(12).bytes(fromHex(b.key)).bytes(fromHex(b.proof)),
};

export const ENTRY_KINDS = Object.keys(BODY);

/** Canonical bytes of an entry (the unsigned part of a signed entry). */
export function entryCanonical(entry) {
  const encode = BODY[entry.body?.type];
  if (!encode) throw new RangeError(`unknown entry type ${entry.body?.type}`);
  const c = new Canonical('ledger-entry')
    .fixed32(entry.chain)
    .fixed32(entry.author)
    .u64(entry.timestamp_ms)
    .u64(entry.nonce);
  encode(c, entry.body);
  return c.finish();
}

export const entryId = (entry) => toHex(digest('entry-id', entryCanonical(entry)));

/**
 * Verify a signed entry against the chain: right chain, author signature
 * over the canonical bytes. Throws with the reason; returns the entry id.
 */
export async function verifyEntry(signed, chainId) {
  const entry = signed.entry;
  if (entry.chain !== chainId) throw new Error('entry is for another chain');
  const canonical = entryCanonical(entry);
  if (!(await verify(entry.author, canonical, signed.signature))) {
    throw new Error('entry signature does not verify');
  }
  return toHex(digest('entry-id', canonical));
}

export { big };
