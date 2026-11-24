// Read DarmRegistry from any EVM chain over plain JSON-RPC (eth_call), with no
// library. A device that knows a DARM chain id and the operator's admin
// address can find the network's archive list on-chain, without relying on
// the web host that served this page.
//
// The registry is first come, first served, so a result only counts when the
// registered admin equals the address pinned in the site config.

const SELECTOR = '0x361885f6'; // networkInfo(bytes32), checked against the compiled ABI in tests
const HEX32 = /^(0x)?[0-9a-fA-F]{64}$/;
const ADDRESS = /^0x[0-9a-fA-F]{40}$/;
const word = (hex, i) => hex.slice(i * 64, (i + 1) * 64);
const num = (hex, label) => {
  const n = BigInt('0x' + hex);
  if (n > 0xffffffffn) throw new Error(`registry reply: ${label} out of range`);
  return Number(n);
};

export function encodeNetworkInfoCall(chainId) {
  if (!HEX32.test(chainId)) throw new Error('chain id must be 32 bytes of hex');
  return SELECTOR + chainId.replace(/^0x/, '').toLowerCase();
}

/** Decode `networkInfo` output with bounds checks on every offset and length. */
export function decodeNetworkInfo(output) {
  const hex = output.replace(/^0x/, '');
  if (hex.length < 5 * 64 || hex.length % 2) throw new Error('registry reply too short');
  const at = (offsetBytes) => {
    if (offsetBytes % 32 || offsetBytes * 2 + 64 > hex.length) throw new Error('registry reply: bad offset');
    return offsetBytes * 2;
  };
  const admin = '0x' + word(hex, 0).slice(24);
  const genesis = word(hex, 1);
  const version = num(word(hex, 2), 'version');
  const updatedAt = num(word(hex, 3), 'updatedAt');
  const arrayBase = at(num(word(hex, 4), 'offset'));
  const count = num(hex.slice(arrayBase, arrayBase + 64), 'count');
  if (count > 8) throw new Error('registry reply: too many urls');
  const urls = [];
  const dataStart = arrayBase + 64;
  for (let i = 0; i < count; i++) {
    const rel = num(hex.slice(dataStart + i * 64, dataStart + i * 64 + 64), 'url offset');
    const start = dataStart + at(rel); // offsets are relative to the first element
    if (start + 64 > hex.length) throw new Error('registry reply: bad url offset');
    const length = num(hex.slice(start, start + 64), 'url length');
    if (length > 128 || start + 64 + length * 2 > hex.length) throw new Error('registry reply: bad url length');
    const bytes = hex.slice(start + 64, start + 64 + length * 2).match(/../g) ?? [];
    urls.push(new TextDecoder('utf-8', { fatal: true }).decode(Uint8Array.from(bytes, (b) => parseInt(b, 16))));
  }
  return { admin: admin.toLowerCase(), genesis, version, updatedAt, urls };
}

/**
 * Ask one registry. Returns null unless the on-chain admin (and genesis, when
 * pinned) match what the site config pins.
 */
export async function readRegistry(entry, { fetchFn = fetch, timeoutMs = 8000 } = {}) {
  if (!ADDRESS.test(entry.registry ?? '') || !ADDRESS.test(entry.admin ?? '') || !HEX32.test(entry.chainId ?? '') || !/^https:\/\//.test(entry.rpc ?? '')) return null;
  const controller = new AbortController();
  const timer = setTimeout(() => controller.abort(), timeoutMs);
  try {
    const res = await fetchFn(entry.rpc, {
      method: 'POST',
      headers: { 'content-type': 'application/json' },
      body: JSON.stringify({ jsonrpc: '2.0', id: 1, method: 'eth_call', params: [{ to: entry.registry, data: encodeNetworkInfoCall(entry.chainId) }, 'latest'] }),
      signal: controller.signal,
      credentials: 'omit',
    });
    const body = await res.json();
    if (!res.ok || typeof body.result !== 'string') return null;
    const info = decodeNetworkInfo(body.result);
    if (info.admin !== entry.admin.toLowerCase()) return null;
    if (entry.genesis && info.genesis !== entry.genesis.replace(/^0x/, '').toLowerCase()) return null;
    return { ...info, source: entry.name ?? entry.rpc };
  } catch {
    return null;
  } finally {
    clearTimeout(timer);
  }
}

/** Archive URLs from every configured registry, deduplicated, first chain first. */
export async function onChainArchives(entries = [], opts) {
  const results = await Promise.all(entries.map((e) => readRegistry(e, opts)));
  const out = [];
  results.forEach((info, i) => {
    if (!info) return;
    for (const url of info.urls) if (!out.some((o) => o.url === url)) out.push({ url, genesis: info.genesis, via: info.source ?? entries[i].name });
  });
  return out;
}
