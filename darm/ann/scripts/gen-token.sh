#!/usr/bin/env bash
# Generate a 256-bit bearer token for darm-node writes.
#   scripts/gen-token.sh path/to/token   # writes the file with mode 0600
#   scripts/gen-token.sh -               # prints to stdout
set -euo pipefail
token="$(head -c 32 /dev/urandom | od -An -tx1 | tr -d ' \n')"
if [[ "${1:-}" == "-" ]]; then
  printf '%s' "$token"
  exit 0
fi
out="${1:?usage: gen-token.sh <path>|-}"
umask 077
mkdir -p "$(dirname "$out")"
printf '%s' "$token" > "$out"
echo "wrote $out"
