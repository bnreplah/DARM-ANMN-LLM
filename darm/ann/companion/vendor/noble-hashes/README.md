# Vendored @noble/hashes

BLAKE3 (with the derive-key mode DARM-ANN's hash is defined on) from
[`@noble/hashes`](https://github.com/paulmillr/noble-hashes) **2.4.0**, MIT,
copied unmodified together with the files it imports. It is vendored so the
companion app has no install step and no runtime dependency fetch.

- npm integrity: `sha512-X5XaVWZIBCT7HHZGm5I7ZQXDwLG+bGXuSrMQAW+7Zvl87h1kmc1ZB1VSRJcpUfoUrGQp4Fkoxm5kZ+Ms+aW+eA==`
  (published from GitHub Actions with provenance).
- `SHA256SUMS` lists the hash of every file here; `test/vendor.test.js`
  fails if any of them changes, and the BLAKE3 output is also pinned to the
  protocol's test vectors.

To update: `npm pack @noble/hashes@<version>`, copy the same files, refresh
`SHA256SUMS` and this note, and rerun the tests.
