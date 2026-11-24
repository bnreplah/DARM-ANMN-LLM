import { FORM_FACTORS, WORKLOADS, normalizeArchive, validateInfo, envFile, joinScript } from './kit.js';

const $ = (id) => document.getElementById(id);
let network = null;
let archive = null;

function fail(message) {
  const el = $('error');
  el.textContent = message;
  el.hidden = !message;
}

for (const f of FORM_FACTORS) {
  const o = document.createElement('option');
  o.value = o.textContent = f;
  if (f === 'laptop') o.selected = true;
  $('form-factor').append(o);
}
for (const w of WORKLOADS) {
  const label = document.createElement('label');
  label.className = 'row';
  const box = document.createElement('input');
  box.type = 'checkbox';
  box.value = w;
  box.checked = w === 'inference' || w === 'embedding' || w === 'verification';
  label.append(box, ` ${w}`);
  $('workloads').append(label);
}
$('share').addEventListener('input', () => {
  $('share-out').textContent = $('share').value;
});

fetch('config.json')
  .then((r) => (r.ok ? r.json() : {}))
  .then((c) => {
    if (c.archive) $('archive').value = c.archive;
  })
  .catch(() => {});

$('lookup').addEventListener('submit', async (event) => {
  event.preventDefault();
  fail('');
  $('network').hidden = true;
  $('downloads').hidden = true;
  try {
    archive = normalizeArchive($('archive').value);
    const res = await fetch(`${archive}/v1/info`);
    if (!res.ok) throw new Error(`The archive answered ${res.status}. If it needs a read token or does not allow this site's origin, ask the operator.`);
    network = validateInfo(await res.json());
    $('n-chain').textContent = network.chain;
    $('n-genesis').textContent = network.genesis;
    $('n-validators').textContent = `${network.validators.length} genesis validator(s)`;
    $('n-height').textContent = network.height;
    $('network').hidden = false;
  } catch (error) {
    fail(error instanceof TypeError ? 'Could not reach the archive. Check the URL and that it allows this site (DARM_NODE_CORS_ORIGINS).' : error.message);
  }
});

function download(name, text) {
  const url = URL.createObjectURL(new Blob([text], { type: 'text/plain' }));
  const a = document.createElement('a');
  a.href = url;
  a.download = name;
  a.click();
  URL.revokeObjectURL(url);
}

$('choices').addEventListener('submit', (event) => {
  event.preventDefault();
  fail('');
  try {
    const choices = {
      formFactor: $('form-factor').value,
      workloads: [...$('workloads').querySelectorAll('input:checked')].map((b) => b.value),
      sharePct: $('share').value,
      requireCharging: $('charging').checked,
    };
    const env = envFile(network, choices, archive);
    download('darm-join.env', env);
    download('join.sh', joinScript(network));
    $('downloads').hidden = false;
  } catch (error) {
    fail(error.message);
  }
});
