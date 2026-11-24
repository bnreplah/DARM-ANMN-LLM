// Canonical encoding (spec §2). Integers are big-endian; u64 values are
// BigInt or a safe Number. Floating point never appears.

import { concat, fromHex, utf8 } from './bytes.js';

export const PROTOCOL = 'DARM-ANN/v1';

export class Canonical {
  constructor(domain) {
    this.parts = [];
    this.str(`${PROTOCOL}/${domain}`);
  }

  #push(bytes) {
    this.parts.push(bytes);
    return this;
  }

  u8(v) {
    if (!Number.isInteger(v) || v < 0 || v > 0xff) throw new RangeError('u8');
    return this.#push(Uint8Array.of(v));
  }

  bool(v) {
    return this.u8(v ? 1 : 0);
  }

  u16(v) {
    if (!Number.isInteger(v) || v < 0 || v > 0xffff) throw new RangeError('u16');
    return this.#push(Uint8Array.of(v >> 8, v & 0xff));
  }

  u32(v) {
    if (!Number.isInteger(v) || v < 0 || v > 0xffffffff) throw new RangeError('u32');
    const out = new Uint8Array(4);
    new DataView(out.buffer).setUint32(0, v);
    return this.#push(out);
  }

  i32(v) {
    if (!Number.isInteger(v) || v < -0x80000000 || v > 0x7fffffff) throw new RangeError('i32');
    const out = new Uint8Array(4);
    new DataView(out.buffer).setInt32(0, v);
    return this.#push(out);
  }

  u64(v) {
    const big = BigInt(v);
    if (big < 0n || big > 0xffffffffffffffffn) throw new RangeError('u64');
    const out = new Uint8Array(8);
    new DataView(out.buffer).setBigUint64(0, big);
    return this.#push(out);
  }

  bytes(value) {
    this.u32(value.length);
    return this.#push(Uint8Array.from(value));
  }

  str(value) {
    return this.bytes(utf8(value));
  }

  fixed32(value) {
    const bytes = typeof value === 'string' ? fromHex(value, 32) : value;
    if (bytes.length !== 32) throw new RangeError('fixed32 needs 32 bytes');
    return this.#push(Uint8Array.from(bytes));
  }

  optFixed32(value) {
    if (value === null || value === undefined) return this.u8(0);
    this.u8(1);
    return this.fixed32(value);
  }

  finish() {
    return concat(...this.parts);
  }
}
