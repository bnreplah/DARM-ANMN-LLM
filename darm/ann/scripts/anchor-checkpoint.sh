#!/usr/bin/env bash
# Approve the archive's latest finalized checkpoint on the DarmAnchor
# contract, as one validator. Run by each validator operator; the checkpoint
# is anchored once more than two thirds have approved identical values.
#
# Requires: curl, jq, Foundry's `cast`.
#   DARM_ARCHIVE=https://archive.example   (http://127.0.0.1:8088 locally)
#   DARM_ANCHOR=0x...                       contract address
#   ETH_RPC_URL=https://...                 RPC endpoint
#   ETH_KEYSTORE=path/to/keystore.json      encrypted key; cast prompts for the password
#
# The script never takes a raw private key on the command line (it would land
# in shell history and process listings).
set -euo pipefail
: "${DARM_ARCHIVE:?}" "${DARM_ANCHOR:?}" "${ETH_RPC_URL:?}" "${ETH_KEYSTORE:?}"

case "$DARM_ARCHIVE" in
  https://*|http://127.0.0.1*|http://localhost*) ;;
  *) echo "refusing plaintext HTTP to a remote archive; use https://" >&2; exit 1 ;;
esac

cp_json="$(curl --fail --silent --show-error --max-time 30 "$DARM_ARCHIVE/v1/ledger/checkpoint")"
chain_id="$(jq -r .chain_id <<<"$cp_json")"
height="$(jq -r .height <<<"$cp_json")"
block_hash="$(jq -r .block_hash <<<"$cp_json")"
entries_root="$(jq -r .entries_root <<<"$cp_json")"
for v in "$chain_id" "$block_hash" "$entries_root"; do
  [[ "$v" =~ ^0x[0-9a-f]{64}$ ]] || { echo "malformed checkpoint field: $v" >&2; exit 1; }
done
[[ "$height" =~ ^[0-9]+$ ]] || { echo "malformed height: $height" >&2; exit 1; }

echo "Approving chain $chain_id height $height block $block_hash"
echo "Verify these against your own mesh node's darm_status before confirming."
read -r -p "Continue? [y/N] " answer
[[ "$answer" == "y" || "$answer" == "Y" ]] || exit 1

cast send "$DARM_ANCHOR" \
  "approveCheckpoint(bytes32,uint64,bytes32,bytes32)" \
  "$chain_id" "$height" "$block_hash" "$entries_root" \
  --rpc-url "$ETH_RPC_URL" --keystore "$ETH_KEYSTORE"
