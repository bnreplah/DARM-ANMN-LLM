// Device identity: an Ed25519 key pair held by the platform's WebCrypto.
//
// The private key is created non-extractable, so the app (and any script that
// ever runs in its origin) can ask it to sign but can never read it out. The
// node id is the public key, exactly as for every DARM-ANN node (spec §1).

import { equal, fromHex, toHex } from './bytes.js';

const ALGORITHM = { name: 'Ed25519' };

/** Whether this browser can do Ed25519 in WebCrypto (Safari 17+, Chrome 137+, Firefox 129+). */
export async function ed25519Supported() {
  try {
    await globalThis.crypto.subtle.generateKey(ALGORITHM, false, ['sign', 'verify']);
    return true;
  } catch {
    return false;
  }
}

async function publicHex(publicKey) {
  const raw = await globalThis.crypto.subtle.exportKey('raw', publicKey);
  return toHex(new Uint8Array(raw));
}

/** A fresh identity. The private key is not extractable. */
export async function createIdentity() {
  const pair = await globalThis.crypto.subtle.generateKey(ALGORITHM, false, ['sign', 'verify']);
  return { publicKey: pair.publicKey, privateKey: pair.privateKey, nodeId: await publicHex(pair.publicKey) };
}

/** Rebuild an identity from stored CryptoKeys (e.g. read back from IndexedDB). */
export async function identityFromKeys({ publicKey, privateKey }) {
  return { publicKey, privateKey, nodeId: await publicHex(publicKey) };
}

const PKCS8_PREFIX = fromHex('302e020100300506032b657004220420');

/**
 * An identity from a 32-byte seed. For tests and for recovery tooling; the
 * app itself never handles the seed of the identity it creates.
 */
export async function identityFromSeed(seed) {
  if (seed.length !== 32) throw new RangeError('seed must be 32 bytes');
  const pkcs8 = new Uint8Array(PKCS8_PREFIX.length + 32);
  pkcs8.set(PKCS8_PREFIX);
  pkcs8.set(seed, PKCS8_PREFIX.length);
  const subtle = globalThis.crypto.subtle;
  const privateKey = await subtle.importKey('pkcs8', pkcs8, ALGORITHM, true, ['sign']);
  const jwk = await subtle.exportKey('jwk', privateKey);
  const publicKey = await subtle.importKey(
    'jwk',
    { kty: jwk.kty, crv: jwk.crv, x: jwk.x },
    ALGORITHM,
    true,
    ['verify'],
  );
  return { publicKey, privateKey, nodeId: await publicHex(publicKey) };
}

/** Sign `message`; returns the protocol's tagged form `ed25519:<hex>`. */
export async function sign(identity, message) {
  const signature = await globalThis.crypto.subtle.sign(ALGORITHM, identity.privateKey, message);
  return `ed25519:${toHex(new Uint8Array(signature))}`;
}

// y-coordinates (mod p) of the eight small-order points of edcurve25519:
// y = 0 (order 4), y = 1 (order 1), y = p - 1 (order 2) and the order-8 pair.
const P = (1n << 255n) - 19n;

// Little-endian encodings of the two order-8 points (the y values are
// negatives of each other mod p), as listed by libsodium's blocklist.
const ORDER_8 = [
  '26e8958fc2b227b045c3f489f2ef98f0d5dfac05d3c63339b13802886d53fc05',
  'c7176a703d4dd84fba3c0b760d10670f2a2053fa2c39ccc64ec7fd7792ac037a',
];

function yOf(bytes) {
  let y = 0n;
  for (let i = 31; i >= 0; i--) y = (y << 8n) | BigInt(i === 31 ? bytes[i] & 0x7f : bytes[i]);
  return y % P;
}

export const SMALL_ORDER_Y = new Set([0n, 1n, P - 1n, ...ORDER_8.map((h) => yOf(fromHex(h)))]);

/** True for keys of small order, which `verify_strict` rejects (spec §1). */
export function isSmallOrder(publicBytes) {
  if (publicBytes.length !== 32) return true;
  return SMALL_ORDER_Y.has(yOf(publicBytes));
}

const keyCache = new Map();

async function importPublic(nodeHex) {
  let key = keyCache.get(nodeHex);
  if (!key) {
    key = await globalThis.crypto.subtle.importKey('raw', fromHex(nodeHex, 32), ALGORITHM, false, ['verify']);
    if (keyCache.size > 4096) keyCache.clear();
    keyCache.set(nodeHex, key);
  }
  return key;
}

/**
 * Verify an `ed25519:<hex>` signature the way the protocol requires: WebCrypto
 * enforces a canonical signature, and keys of small order are refused here
 * because WebCrypto cannot be asked to (spec §1, `verify_strict`).
 */
export async function verify(nodeHex, message, tagged) {
  if (typeof tagged !== 'string' || !tagged.startsWith('ed25519:') || tagged.length !== 8 + 128) {
    return false;
  }
  let signature;
  let publicBytes;
  try {
    signature = fromHex(tagged.slice(8), 64);
    publicBytes = fromHex(nodeHex, 32);
  } catch {
    return false;
  }
  if (isSmallOrder(publicBytes)) return false;
  try {
    return await globalThis.crypto.subtle.verify(ALGORITHM, await importPublic(nodeHex), signature, message);
  } catch {
    return false;
  }
}

export const sameNode = (a, b) => equal(fromHex(a, 32), fromHex(b, 32));
