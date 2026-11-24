// Merkle trees (spec §4.5): an unpaired last node is promoted unchanged.

import { concat, fromHex, toHex } from './bytes.js';
import { digest } from './hash.js';

const leaf = (id) => digest('merkle-leaf', id);
const node = (l, r) => digest('merkle-node', concat(l, r));

export function root(ids) {
  if (ids.length === 0) return digest('merkle-empty', new Uint8Array(0));
  let level = ids.map((id) => leaf(typeof id === 'string' ? fromHex(id, 32) : id));
  while (level.length > 1) {
    const next = [];
    for (let i = 0; i < level.length; i += 2) {
      next.push(i + 1 < level.length ? node(level[i], level[i + 1]) : level[i]);
    }
    level = next;
  }
  return level[0];
}

export const rootHex = (ids) => toHex(root(ids));

/**
 * Check an inclusion proof as the archive serves it: `path` is a list of
 * `[side, siblingHex]`, where `side` says on which side the sibling sits.
 */
export function verifyProof(id, proof, expectedRootHex) {
  let acc = leaf(typeof id === 'string' ? fromHex(id, 32) : id);
  for (const [side, siblingHex] of proof.path) {
    const sibling = fromHex(siblingHex, 32);
    if (side !== 'left' && side !== 'right') return false;
    acc = side === 'left' ? node(sibling, acc) : node(acc, sibling);
  }
  return toHex(acc) === expectedRootHex;
}
