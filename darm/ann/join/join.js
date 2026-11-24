import { FORM_FACTORS, WORKLOADS, DEFAULT_WORKLOADS, PROFILES, guessFormFactor, isMobile, envFile, joinScript } from './kit.js';
import { loadSettings, saveSettings, clearSettings, candidateArchives } from './settings.js';
import { connect } from './net.js';

const $ = (id) => document.getElementById(id);
const storage = (kind) => {
  try {
    return window[kind];
  } catch {
    return null;
  }
};
const local = storage('localStorage');
const session = storage('sessionStorage');

let config = {};
let network = null;
let archive = null;

function status(message, kind = '') {
  const el = $('status');
  el.textContent = message;
  el.className = `status ${kind}`;
}
function fail(message) {
  const el = $('error');
  el.textContent = message;
  el.hidden = !message;
}

// Customize: device type and workloads.
const guess = guessFormFactor(navigator.userAgent);
for (const f of FORM_FACTORS) {
  const o = document.createElement('option');
  o.value = o.textContent = f;
  o.selected = f === guess;
  $('form-factor').append(o);
}
for (const w of WORKLOADS) {
  const label = document.createElement('label');
  label.className = 'row';
  const box = document.createElement('input');
  box.type = 'checkbox';
  box.value = w;
  box.checked = DEFAULT_WORKLOADS.includes(w);
  label.append(box, ` ${w}`);
  $('workloads').append(label);
}

// Contribution levels.
for (const [key, p] of Object.entries(PROFILES)) {
  const label = document.createElement('label');
  const radio = document.createElement('input');
  radio.type = 'radio';
  radio.name = 'profile';
  radio.value = key;
  radio.checked = key === (isMobile(navigator.userAgent) ? 'light' : 'balanced');
  const text = document.createElement('span');
  text.textContent = p.label;
  const small = document.createElement('small');
  small.textContent = p.detail;
  text.append(small);
  label.append(radio, text);
  $('profiles').append(label);
}

async function start() {
  fail('');
  $('ready').hidden = true;
  status('Finding the network…');
  const settings = loadSettings(local, session);
  const candidates = candidateArchives(settings, config);
  if (!candidates.length) {
    status('No default network is configured on this site. Open Settings and enter your archive URL.', 'bad');
    $('settings').open = true;
    return;
  }
  try {
    const result = await connect(candidates, { readToken: settings.readToken });
    network = result.network;
    archive = result.archive;
    status(`Connected to ${network.chain}.`, 'ok');
    $('lead').textContent = isMobile(navigator.userAgent)
      ? 'You are on a phone or tablet. The phone app is the easiest way to take part; the kit below is for computers and edge boxes.'
      : 'Pick how much of this device to share, then download your kit.';
    $('n-chain').textContent = network.chain;
    $('n-via').textContent = new URL(archive).host + (result.tried > 1 ? ' (a fallback)' : '');
    $('n-genesis').textContent = network.genesis;
    $('n-validators').textContent = `${network.validators.length} genesis validator(s)`;
    $('n-height').textContent = network.height;
    $('ready').hidden = false;
  } catch (error) {
    const f = error.failures ?? [];
    const needsToken = f.some((x) => x.reason === 'needs a read token');
    status(
      needsToken
        ? 'This archive needs a read token. Open Settings and enter it.'
        : 'Could not reach the network. Check your connection, or open Settings to use your own archive.',
      'bad',
    );
    if (needsToken) $('settings').open = true;
  }
}

function download(name, text) {
  const url = URL.createObjectURL(new Blob([text], { type: 'text/plain' }));
  const a = document.createElement('a');
  a.href = url;
  a.download = name;
  a.click();
  URL.revokeObjectURL(url);
}

$('get-kit').addEventListener('click', () => {
  fail('');
  try {
    const profile = PROFILES[document.querySelector('input[name=profile]:checked').value];
    const choices = {
      formFactor: $('form-factor').value,
      workloads: [...$('workloads').querySelectorAll('input:checked')].map((b) => b.value),
      sharePct: profile.sharePct,
      requireCharging: profile.requireCharging,
    };
    download('darm-join.env', envFile(network, choices, archive));
    download('join.sh', joinScript(network));
    $('downloads').hidden = false;
  } catch (error) {
    fail(error.message);
  }
});

$('settings-form').addEventListener('submit', (event) => {
  event.preventDefault();
  saveSettings(
    { archive: $('archive').value.trim(), readToken: $('read-token').value, enrollToken: $('enroll-token').value, remember: $('remember').checked },
    local,
    session,
  );
  start();
});
$('reset').addEventListener('click', () => {
  clearSettings(local, session);
  for (const id of ['archive', 'read-token', 'enroll-token']) $(id).value = '';
  $('remember').checked = false;
  start();
});

(async () => {
  try {
    const res = await fetch('config.json');
    if (res.ok) config = await res.json();
  } catch {
    /* no site config: the user can still enter an archive in Settings */
  }
  const saved = loadSettings(local, session);
  $('archive').value = saved.archive;
  $('read-token').value = saved.readToken;
  $('enroll-token').value = saved.enrollToken;
  $('remember').checked = saved.remember;
  start();
})();
