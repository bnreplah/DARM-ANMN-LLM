// Device protocol messages (spec §11). Each one is a canonical encoding with
// its own domain, so a signature made for one purpose can never be replayed
// as another, and every one binds the chain id.

import { randomBytes, toHex, utf8 } from './bytes.js';
import { Canonical } from './canonical.js';
import { digest } from './hash.js';
import { sign } from './identity.js';

export const KIND_LEDGER_VERIFY = 'ledger-verify';
export const PLATFORMS = ['ios', 'android', 'web', 'desktop', 'other'];
const VERDICT_CODE = { ok: 1, invalid: 2 };

export const bodyHash = (body) => digest('device-body', typeof body === 'string' ? utf8(body) : body);

export function enrollMessage({ chainId, node, platform, label, createdMs }) {
  return new Canonical('device-enroll')
    .fixed32(chainId)
    .fixed32(node)
    .str(platform)
    .str(label)
    .u64(createdMs)
    .finish();
}

export function requestMessage({ chainId, node, method, target, timestampMs, nonce, body }) {
  return new Canonical('device-request')
    .fixed32(chainId)
    .fixed32(node)
    .str(method)
    .str(target)
    .u64(timestampMs)
    .bytes(nonce)
    .fixed32(bodyHash(body))
    .finish();
}

export function workMessage({ chainId, node, claim }) {
  const code = VERDICT_CODE[claim.verdict];
  if (!code) throw new RangeError('unknown verdict');
  return new Canonical('device-work')
    .fixed32(chainId)
    .fixed32(node)
    .str(claim.kind)
    .u64(claim.from_height)
    .u64(claim.to_height)
    .fixed32(claim.head)
    .u8(code)
    .u32(claim.elapsed_ms)
    .u64(claim.completed_ms)
    .finish();
}

/** The JSON body of `POST /v1/devices`. */
export async function buildEnrollment(identity, { chainId, platform, label = '', createdMs }) {
  if (!PLATFORMS.includes(platform)) throw new RangeError('unknown platform');
  const message = enrollMessage({ chainId, node: identity.nodeId, platform, label, createdMs });
  return {
    node: identity.nodeId,
    platform,
    label,
    created_ms: createdMs,
    signature: await sign(identity, message),
  };
}

/**
 * Headers and body for a request that acts for the device. `target` is the
 * request target exactly as the archive sees it (path and query).
 */
export async function signedRequest(identity, { chainId, method, target, body = '', nowMs, nonce }) {
  const n = nonce ?? randomBytes(16);
  const message = requestMessage({
    chainId,
    node: identity.nodeId,
    method,
    target,
    timestampMs: nowMs,
    nonce: n,
    body,
  });
  return {
    headers: {
      'content-type': 'application/json',
      'x-darm-node': identity.nodeId,
      'x-darm-timestamp': String(nowMs),
      'x-darm-nonce': toHex(n),
      'x-darm-signature': await sign(identity, message),
    },
    body,
  };
}

/** A signed verification receipt, in the JSON shape the archive stores. */
export async function signWork(identity, { chainId, claim }) {
  const message = workMessage({ chainId, node: identity.nodeId, claim });
  return { node: identity.nodeId, ...claim, signature: await sign(identity, message) };
}
