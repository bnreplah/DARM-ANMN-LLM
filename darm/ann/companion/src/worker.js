// The companion's engine: pairs a device identity with an archive, verifies
// the ledger it serves, derives the wallet locally and submits signed audit
// receipts. Network, storage and clock are injected so it runs the same in a
// browser, in tests and in a service worker.
//
// Trust model: the user pins the chain (name + genesis validators) the first
// time they connect and sees its genesis hash. After that the archive cannot
// substitute another chain, and every block is checked against the pinned
// validators and their recorded changes before it counts.

import { genesisHash, ChainVerifier, chainIdOf } from './ledger.js';
import { buildEnrollment, signWork, signedRequest, KIND_LEDGER_VERIFY } from './protocol.js';
import { Ledger } from './wallet.js';
import { parseJson, small } from './json.js';

const PAGE = 32;

export class Companion {
  /** @param {{identity, store, fetch: typeof fetch, now: () => number, platform?: string}} deps */
  constructor({ identity, store, fetch: fetchFn, now = Date.now, platform = 'web' }) {
    this.identity = identity;
    this.store = store;
    this.http = fetchFn;
    this.now = now;
    this.platform = platform;
    this.verifier = null;
    this.wallet = new Ledger();
    this.baseUrl = null;
    this.enrollToken = null;
  }

  get nodeId() {
    return this.identity.nodeId;
  }

  async #get(path) {
    const res = await this.http(this.baseUrl + path, { headers: this.readHeaders ?? {} });
    const text = await res.text();
    if (!res.ok) throw new Error(`${path}: ${res.status} ${text.slice(0, 120)}`);
    return parseJson(text);
  }

  /**
   * Look at an archive without trusting it. Returns the chain it claims so the
   * user can compare the genesis hash to one given by the operator.
   */
  async discover(baseUrl, { readToken } = {}) {
    this.baseUrl = baseUrl.replace(/\/+$/, '');
    this.readHeaders = readToken ? { authorization: `Bearer ${readToken}` } : {};
    const info = await this.#get('/v1/info');
    const computed = genesisHash(chainIdOf(info.chain), info.genesis_validators.map((m) => ({ node: m.node, weight: m.weight })));
    if (computed !== info.genesis || chainIdOf(info.chain) !== info.chain_id) {
      throw new Error('the archive reports a chain id or genesis that its own validators do not produce');
    }
    return { chain: info.chain, chainId: info.chain_id, genesis: computed, validators: info.genesis_validators, height: info.height };
  }

  /** Pin the chain the user accepted. Refuses to re-pin a different one. */
  async pin(found, { enrollToken } = {}) {
    const existing = await this.store.get('pin');
    if (existing && existing.genesis !== found.genesis) throw new Error('this device is already bound to a different chain');
    const pin = { baseUrl: this.baseUrl, name: found.chain, genesis: found.genesis, validators: found.validators, readHeaders: this.readHeaders };
    await this.store.set('pin', pin);
    if (enrollToken) await this.store.set('enrollToken', enrollToken);
    await this.#load(pin);
  }

  async resume() {
    const pin = await this.store.get('pin');
    if (!pin) return false;
    this.baseUrl = pin.baseUrl;
    this.readHeaders = pin.readHeaders;
    await this.#load(pin);
    return true;
  }

  async #load(pin) {
    this.pinned = pin;
    this.verifier = new ChainVerifier({ name: pin.name, genesisValidators: pin.validators });
    if (this.verifier.genesisHash !== pin.genesis) throw new Error('pinned chain is inconsistent');
    const cp = await this.store.get('checkpoint');
    const wallet = await this.store.get('wallet');
    if (cp && wallet) {
      this.verifier.restore(cp);
      this.wallet = Ledger.fromJSON(wallet);
    }
  }

  /** Self-signed enrollment of this device's key with the archive. */
  async enroll(label = '') {
    const body = await buildEnrollment(this.identity, { chainId: this.verifier.chainId, platform: this.platform, label, createdMs: this.now() });
    const token = await this.store.get('enrollToken');
    const res = await this.http(`${this.baseUrl}/v1/devices`, {
      method: 'POST',
      headers: { 'content-type': 'application/json', ...(token ? { authorization: `Bearer ${token}` } : {}) },
      body: JSON.stringify(body),
    });
    if (!res.ok) throw new Error(`enrollment failed: ${res.status}`);
    return parseJson(await res.text());
  }

  /**
   * Verify every block the archive has beyond what is already verified.
   * Stops at the first block that fails and reports it; nothing after it is
   * trusted.
   */
  async sync() {
    const started = this.now();
    const from = this.verifier.height;
    let invalid = null;
    for (;;) {
      const page = await this.#get(`/v1/ledger/blocks?after=${this.verifier.height}&limit=${PAGE}`);
      if (page.length === 0) break;
      for (const block of page) {
        try {
          await this.verifier.verifyBlock(block, { nowMs: this.now() });
        } catch (error) {
          if (error.name !== 'VerificationError') throw error;
          invalid = error;
          break;
        }
        this.wallet.apply(block);
      }
      if (invalid) break;
    }
    await this.store.set('checkpoint', this.verifier.checkpoint());
    await this.store.set('wallet', this.wallet.toJSON());
    return { from, to: this.verifier.height, head: this.verifier.head, invalid, elapsedMs: this.now() - started };
  }

  /**
   * Turn a sync into a signed receipt (an audit contribution) and send it.
   * A receipt is only produced when blocks were actually checked.
   */
  async submitAudit(result) {
    if (result.to <= result.from && !result.invalid) return null;
    const claim = {
      kind: KIND_LEDGER_VERIFY,
      from_height: result.from + 1,
      to_height: result.invalid ? result.invalid.height - 1 : result.to,
      head: result.invalid ? this.verifier.head : result.head,
      verdict: result.invalid ? 'invalid' : 'ok',
      elapsed_ms: Math.min(result.elapsedMs, 0xffffffff),
      completed_ms: this.now(),
    };
    if (claim.to_height < claim.from_height && claim.verdict === 'ok') return null;
    const work = await signWork(this.identity, { chainId: this.verifier.chainId, claim });
    const target = `/v1/devices/${this.nodeId}/receipts`;
    const body = JSON.stringify(work);
    const { headers } = await signedRequest(this.identity, { chainId: this.verifier.chainId, method: 'POST', target, body, nowMs: this.now() });
    const res = await this.http(this.baseUrl + target, { method: 'POST', headers, body });
    const text = await res.text();
    if (!res.ok) throw new Error(`receipt rejected: ${res.status} ${text.slice(0, 120)}`);
    return parseJson(text);
  }

  /** Locally derived wallet next to the gateway's claim, with any disagreement. */
  async walletView() {
    const derived = this.wallet.summary(this.nodeId);
    let reported = null;
    let mismatches = [];
    try {
      reported = await this.#get(`/v1/devices/${this.nodeId}/wallet`);
      if (small(reported.ledger.height) === this.verifier.height) {
        mismatches = Ledger.mismatches(reported.credits, derived);
      }
    } catch {
      /* offline or not enrolled: the derived numbers still stand */
    }
    return {
      derived,
      reported,
      mismatches,
      height: this.verifier.height,
      activity: this.wallet.activityFor(this.nodeId),
    };
  }
}
