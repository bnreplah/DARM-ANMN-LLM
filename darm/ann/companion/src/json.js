// The archive serves u64 values (entry nonces are random 64-bit numbers) as
// JSON numbers, and JSON.parse would round anything above 2^53, which breaks
// every signature over it. Integers of 16+ digits are quoted before parsing,
// so they arrive as exact digit strings; `u64()` and `big()` accept those.
//
// The rewrite is purely textual. If it ever touched text inside a string the
// signed content would change and verification would fail closed.

export function parseJson(text) {
  return JSON.parse(text.replace(/([:[,]\s*)(-?\d{16,})(?=\s*[,}\]])/g, '$1"$2"'));
}

/** An exact BigInt from a Number or a digit string. */
export function big(value) {
  if (typeof value === 'bigint') return value;
  if (typeof value === 'number') {
    if (!Number.isSafeInteger(value)) throw new RangeError('integer is not exact');
    return BigInt(value);
  }
  if (typeof value === 'string' && /^-?\d+$/.test(value)) return BigInt(value);
  throw new TypeError('expected an integer');
}

/** A Number for values that are small by protocol (heights, counts). */
export function small(value) {
  const n = Number(big(value));
  if (!Number.isSafeInteger(n)) throw new RangeError('integer too large');
  return n;
}
