// DARM-ANN hash (spec §1): BLAKE3 in derive-key mode,
// H(domain, data) = BLAKE3.derive_key("DARM-ANN/v1 " || domain, data).

import { blake3 } from '../vendor/noble-hashes/blake3.js';
import { toHex, utf8 } from './bytes.js';
import { PROTOCOL } from './canonical.js';

export function digest(domain, data) {
  return blake3(data, { context: utf8(`${PROTOCOL} ${domain}`), dkLen: 32 });
}

export const digestHex = (domain, data) => toHex(digest(domain, data));
