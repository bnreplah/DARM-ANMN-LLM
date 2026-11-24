import { normalizeArchive, validateInfo } from './kit.js';

/**
 * Try each candidate archive in order and return the first that answers with
 * a well-formed /v1/info. A user-chosen archive that fails is reported, not
 * silently replaced, because pointing at a different cluster than the one the
 * user typed would be surprising.
 */
export async function connect(candidates, { readToken = '', fetchFn = fetch, timeoutMs = 8000 } = {}) {
  const failures = [];
  for (const [index, raw] of candidates.entries()) {
    let archive;
    try {
      archive = normalizeArchive(raw);
      const controller = new AbortController();
      const timer = setTimeout(() => controller.abort(), timeoutMs);
      const res = await fetchFn(`${archive}/v1/info`, {
        headers: readToken ? { authorization: `Bearer ${readToken}` } : {},
        signal: controller.signal,
        credentials: 'omit',
      }).finally(() => clearTimeout(timer));
      if (res.status === 401 || res.status === 403) throw new Error('needs a read token');
      if (!res.ok) throw new Error(`answered ${res.status}`);
      return { archive, network: validateInfo(await res.json()), tried: index + 1, failures };
    } catch (error) {
      failures.push({ archive: raw, reason: error.message });
    }
  }
  const err = new Error('no archive answered');
  err.failures = failures;
  throw err;
}
