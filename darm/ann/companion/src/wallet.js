// Credit accounting derived from verified blocks (spec §4.7).
//
// The wallet never trusts a balance a server reports. It folds the entries of
// blocks that ChainVerifier accepted into per-node totals and the device
// compares them with what the gateway says, so a lying gateway is visible.
// Credits are closed-loop: they are granted by governance and moved only by a
// compute settlement, so granted + settled_in - settled_out is the balance.

import { big } from './json.js';

const blank = () => ({ granted: 0n, settled_in: 0n, settled_out: 0n, entries_authored: 0 });

export class Ledger {
  constructor() {
    this.nodes = new Map();
    /** Settlement and grant activity, newest last, for the activity feed. */
    this.activity = [];
  }

  #node(id) {
    let n = this.nodes.get(id);
    if (!n) this.nodes.set(id, (n = blank()));
    return n;
  }

  /** Fold one verified block into the totals. */
  apply(block) {
    const height = Number(block.header.height);
    for (const { entry } of block.entries) {
      this.#node(entry.author).entries_authored += 1;
      const body = entry.body;
      if (body.type === 'credit_grant') {
        const amount = big(body.amount);
        this.#node(body.to).granted += amount;
        this.activity.push({ kind: 'grant', height, to: body.to, amount, proposal: body.proposal });
      } else if (body.type === 'compute_settle') {
        let total = 0n;
        for (const p of body.payments) {
          const amount = big(p.amount);
          total += amount;
          this.#node(p.worker).settled_in += amount;
          this.activity.push({ kind: 'settle', height, job: body.job, worker: p.worker, amount, payer: entry.author });
        }
        this.#node(entry.author).settled_out += total;
      }
    }
  }

  toJSON() {
    const str = (v) => v.toString();
    return {
      nodes: [...this.nodes].map(([id, n]) => [id, { ...n, granted: str(n.granted), settled_in: str(n.settled_in), settled_out: str(n.settled_out) }]),
      activity: this.activity.map((a) => ({ ...a, amount: str(a.amount) })),
    };
  }

  static fromJSON(data) {
    const w = new Ledger();
    for (const [id, n] of data.nodes) {
      w.nodes.set(id, { ...n, granted: big(n.granted), settled_in: big(n.settled_in), settled_out: big(n.settled_out) });
    }
    w.activity = data.activity.map((a) => ({ ...a, amount: big(a.amount) }));
    return w;
  }

  balance(id) {
    const n = this.nodes.get(id) ?? blank();
    return n.granted + n.settled_in - n.settled_out;
  }

  summary(id) {
    const n = this.nodes.get(id) ?? blank();
    return { ...n, balance: n.granted + n.settled_in - n.settled_out, node: id };
  }

  activityFor(id) {
    return this.activity.filter((a) => a.to === id || a.worker === id || a.payer === id);
  }

  /**
   * Compare a gateway-reported summary with the locally derived one. The
   * gateway can legitimately be a block behind, so the caller passes the
   * height both are at.
   */
  static mismatches(reported, derived) {
    const out = [];
    for (const key of ['balance', 'granted', 'settled_in', 'settled_out']) {
      if (big(reported[key]) !== derived[key]) out.push({ key, reported: String(reported[key]), derived: derived[key].toString() });
    }
    return out;
  }
}
