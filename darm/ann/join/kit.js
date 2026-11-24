// Builds the starter kit for a new edge node from what the archive reports.
// Pure functions, no DOM, so they are tested under node.
//
// Everything that ends up in a generated file is validated against a strict
// allow-list first and then single-quoted, so a hostile archive response or a
// typo cannot inject shell.

export const FORM_FACTORS = ['server', 'desktop', 'laptop', 'phone', 'tablet', 'edge'];
export const WORKLOADS = ['inference', 'embedding', 'indexing', 'training', 'verification'];
const HEX64 = /^[0-9a-f]{64}$/;
const CHAIN = /^[A-Za-z0-9._-]{1,64}$/;
const FEATURE = /^[a-z][a-z0-9-]{0,31}$/;

export function normalizeArchive(input) {
  let url;
  try {
    url = new URL(input.trim());
  } catch {
    throw new Error('Enter a full URL such as https://archive.example.org');
  }
  const local = ['localhost', '127.0.0.1'].includes(url.hostname);
  if (url.protocol !== 'https:' && !(local && url.protocol === 'http:')) throw new Error('The archive must use https (http only for localhost).');
  if (url.username || url.password || url.search || url.hash) throw new Error('Remove credentials, query and fragment from the URL.');
  return url.origin + url.pathname.replace(/\/+$/, '');
}

/** Check the shape of /v1/info before any of it is written to a file. */
export function validateInfo(info) {
  if (!info || typeof info.chain !== 'string' || !CHAIN.test(info.chain)) throw new Error('The archive reported an unusable chain name.');
  if (!HEX64.test(info.genesis ?? '')) throw new Error('The archive reported no genesis hash.');
  const validators = info.genesis_validators;
  if (!Array.isArray(validators) || validators.length < 1 || validators.length > 64) throw new Error('The archive reported no genesis validators.');
  for (const v of validators) if (!HEX64.test(v.node ?? '')) throw new Error('A validator id is malformed.');
  const activations = Object.entries(info.activations ?? {});
  for (const [name, height] of activations) {
    if (!FEATURE.test(name) || !Number.isSafeInteger(Number(height)) || Number(height) < 0) throw new Error('An activation entry is malformed.');
  }
  return {
    chain: info.chain,
    genesis: info.genesis,
    validators: validators.map((v) => v.node),
    activations: activations.map(([name, height]) => `${name}@${Number(height)}`),
    height: Number(info.height) || 0,
  };
}

export function validateChoices({ formFactor, workloads, sharePct, requireCharging }) {
  if (!FORM_FACTORS.includes(formFactor)) throw new Error('Unknown device type.');
  if (!workloads.length || !workloads.every((w) => WORKLOADS.includes(w))) throw new Error('Pick at least one workload.');
  const share = Number(sharePct);
  if (!Number.isInteger(share) || share < 1 || share > 100) throw new Error('Share must be 1 to 100.');
  return { formFactor, workloads, sharePct: share, requireCharging: Boolean(requireCharging) };
}

const q = (value) => `'${String(value).replace(/'/g, `'\\''`)}'`;

export function envFile(network, choices, archive) {
  const c = validateChoices(choices);
  const lines = [
    '# DARM-ANN join settings. Generated for this cluster; contains no secrets.',
    `# Genesis hash (compare with your operator's): ${network.genesis}`,
    `export DARM_CHAIN_NAME=${q(network.chain)}`,
    `export DARM_VALIDATORS=${q(network.validators.join(','))}`,
  ];
  if (network.activations.length) lines.push(`export DARM_ACTIVATE=${q(network.activations.join(','))}`);
  lines.push(
    `export DARM_ARCHIVE_URL=${q(archive)}`,
    'export DARM_SHARE=1',
    'export DARM_COMPUTE_POOL=1',
    `export DARM_FORM_FACTOR=${q(c.formFactor)}`,
    `export DARM_WORKLOADS=${q(c.workloads.join(','))}`,
    `export DARM_POOL_SHARE_PCT=${c.sharePct}`,
    `export DARM_POOL_REQUIRE_CHARGING=${c.requireCharging ? 1 : 0}`,
    '',
  );
  return lines.join('\n');
}

export function joinScript(network) {
  return `#!/usr/bin/env bash
# DARM-ANN join script for chain ${network.chain.replace(/[^A-Za-z0-9._-]/g, '')}.
# Creates this machine's identity key (once), shows its node id, and starts a
# mesh-llm node that contributes compute under the settings in darm-join.env.
set -euo pipefail
umask 077
here="$(cd "$(dirname "\${BASH_SOURCE[0]}")" && pwd)"

for tool in mesh-llm darm-node; do
  command -v "$tool" >/dev/null 2>&1 || {
    echo "missing: $tool. Build both from https://github.com/bnreplah/DARM-ANMN-LLM (see docs/QUICKSTART.md)." >&2
    exit 1
  }
done

# shellcheck source=darm-join.env
. "$here/darm-join.env"
export DARM_DATA_DIR="\${DARM_DATA_DIR:-$HOME/.mesh-llm/darm}"
mkdir -p "$DARM_DATA_DIR"
chmod 700 "$DARM_DATA_DIR"

key="$DARM_DATA_DIR/identity.key"
if [ ! -e "$key" ]; then
  echo "Creating this node's identity key at $key"
  node_id="$(darm-node keygen --out "$key")"
else
  echo "Using the existing identity key at $key"
  node_id="(see 'darm_status' once the node is running)"
fi
echo
echo "Your node id: $node_id"
echo "Back up $key. It is your identity and your credits; it cannot be recovered."
echo

if [ -z "\${MESH_JOIN_TOKEN:-}" ]; then
  read -r -p "Mesh join token from your operator (empty to run standalone): " MESH_JOIN_TOKEN
fi
if [ -n "$MESH_JOIN_TOKEN" ]; then
  exec mesh-llm serve --auto --join "$MESH_JOIN_TOKEN"
fi
exec mesh-llm serve --auto
`;
}

/** One-tap contribution levels, so most people never see the raw knobs. */
export const PROFILES = {
  light: { label: 'Light', detail: '25% of the device, only while charging', sharePct: 25, requireCharging: true },
  balanced: { label: 'Balanced', detail: '50% of the device, only while charging', sharePct: 50, requireCharging: true },
  full: { label: 'Full', detail: '100% of the device, any time (best for always-on machines)', sharePct: 100, requireCharging: false },
};
export const DEFAULT_WORKLOADS = ['inference', 'embedding', 'verification'];

/** Guess a sensible device type; the user can change it under Customize. */
export function guessFormFactor(userAgent = '') {
  if (/iPad|Tablet/i.test(userAgent)) return 'tablet';
  if (/iPhone|Android.*Mobile|Mobile/i.test(userAgent)) return 'phone';
  if (/Raspberry|armv|aarch64/i.test(userAgent)) return 'edge';
  return 'laptop';
}

export const isMobile = (userAgent = '') => ['phone', 'tablet'].includes(guessFormFactor(userAgent));
