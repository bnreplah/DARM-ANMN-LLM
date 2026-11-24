import { Companion } from './src/worker.js';
import { createIdentity, identityFromKeys, ed25519Supported } from './src/identity.js';
import { idbStore } from './src/store.js';

const $ = (id) => document.getElementById(id);
const text = (id, value) => {
  $(id).textContent = String(value);
};
const show = (id, on) => {
  $(id).hidden = !on;
};
const banner = (message, kind = '') => {
  const el = $('banner');
  el.textContent = message;
  el.className = `banner ${kind}`;
  el.hidden = !message;
};

let app = null;
let found = null;
let timer = null;

async function loadIdentity() {
  const saved = await idbStore.get('identity');
  if (saved) return identityFromKeys(saved);
  const identity = await createIdentity();
  await idbStore.set('identity', { publicKey: identity.publicKey, privateKey: identity.privateKey });
  return identity;
}

async function render() {
  const view = await app.walletView();
  text('node-id', app.nodeId);
  text('chain', app.pinned.name);
  text('height', view.height);
  text('balance', view.derived.balance);
  text('granted', view.derived.granted);
  text('in', view.derived.settled_in);
  text('out', view.derived.settled_out);
  const warn = $('mismatch');
  if (view.mismatches.length) {
    warn.textContent = `The archive reports different credit totals than the ledger you verified (${view.mismatches.map((m) => m.key).join(', ')}). Trust the numbers above.`;
    warn.hidden = false;
  } else {
    warn.hidden = true;
  }
  const list = $('activity');
  list.replaceChildren();
  for (const a of view.activity.slice(-20).reverse()) {
    const li = document.createElement('li');
    const sign = a.kind === 'grant' ? '+' : a.worker === app.nodeId ? '+' : '-';
    li.textContent = `block ${a.height} · ${a.kind} · ${sign}${a.amount}`;
    list.append(li);
  }
}

async function work() {
  const button = $('work');
  button.disabled = true;
  try {
    const result = await app.sync();
    if (result.invalid) banner(`The archive served a block that does not verify (${result.invalid.reason}). Reporting it.`, 'bad');
    else banner(result.to > result.from ? `Verified blocks ${result.from + 1}–${result.to}.` : 'Already up to date.');
    await app.submitAudit(result);
    await render();
  } catch (error) {
    banner(error.message, 'bad');
  } finally {
    button.disabled = false;
  }
}

async function enterHome() {
  show('pair', false);
  show('home', true);
  await render();
  await work();
}

async function start() {
  if ('serviceWorker' in navigator) navigator.serviceWorker.register('sw.js').catch(() => {});
  if (!(await ed25519Supported())) {
    banner('This browser cannot create the Ed25519 device key. Update to iOS 17 / Safari 17, or Chrome 137 or newer.', 'bad');
    return;
  }
  app = new Companion({
    identity: await loadIdentity(),
    store: idbStore,
    fetch: (...a) => fetch(...a),
    platform: /iPhone|iPad/.test(navigator.userAgent) ? 'ios' : /Android/.test(navigator.userAgent) ? 'android' : 'web',
  });
  if (await app.resume()) return enterHome();
  try {
    const config = await (await fetch('config.json')).json();
    if (config.archive) $('url').value = config.archive;
  } catch {
    /* no preset archive */
  }
  // Settings saved on the join page (same origin) take precedence.
  for (const store of [globalThis.localStorage, globalThis.sessionStorage]) {
    try {
      const saved = JSON.parse(store?.getItem('darm.settings.v1') ?? 'null');
      if (!saved) continue;
      if (saved.archive) $('url').value = saved.archive;
      if (saved.readToken) $('read-token').value = saved.readToken;
      if (saved.enrollToken) $('enroll-token').value = saved.enrollToken;
      break;
    } catch {
      /* unreadable settings are ignored */
    }
  }
  show('pair', true);
}

$('pair-form').addEventListener('submit', async (event) => {
  event.preventDefault();
  try {
    found = await app.discover($('url').value, { readToken: $('read-token').value || undefined });
    text('genesis', found.genesis);
    text('chain-summary', `${found.chain}: ${found.validators.length} genesis validator(s), height ${found.height}.`);
    show('confirm', true);
    banner('');
  } catch (error) {
    banner(error.message, 'bad');
  }
});

$('accept').addEventListener('click', async () => {
  try {
    await app.pin(found, { enrollToken: $('enroll-token').value || undefined });
    await app.enroll($('label').value);
    await enterHome();
  } catch (error) {
    banner(error.message, 'bad');
  }
});

$('work').addEventListener('click', work);

$('auto').addEventListener('change', (event) => {
  clearInterval(timer);
  if (event.target.checked) timer = setInterval(work, 60_000);
});

$('export').addEventListener('click', async () => {
  try {
    const res = await fetch(`${app.baseUrl}/v1/devices/${app.nodeId}/receipts`, { headers: app.readHeaders ?? {} });
    const blob = new Blob([await res.text()], { type: 'application/json' });
    const a = document.createElement('a');
    a.href = URL.createObjectURL(blob);
    a.download = `darm-receipts-${app.nodeId.slice(0, 8)}.json`;
    a.click();
    URL.revokeObjectURL(a.href);
  } catch (error) {
    banner(error.message, 'bad');
  }
});

start().catch((error) => banner(error.message, 'bad'));
