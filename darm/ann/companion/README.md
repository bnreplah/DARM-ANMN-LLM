# DARM-ANN companion

An installable web app (PWA) that turns a phone into a participant in a
DARM-ANN cluster, on iOS and Android, with **no app store, no developer
account and no signing key**. Open the page, pick "Add to Home Screen", and
it runs full-screen with its own icon.

What it does:

- **Identity.** Creates an Ed25519 key in the device. The private key is
  non-extractable (WebCrypto) and never exists as bytes in JavaScript. Your
  **device ID is the public key**, a native ledger node id. Nothing to sign
  up for.
- **Access without secrets.** Enrollment is signed by the key, and every
  request that acts for you is signed over its method, target, body, time and
  a nonce, so it cannot be replayed or redirected. See spec §11.
- **Compute contribution (verifier + audit worker).** Downloads the ledger
  from the archive, verifies every block (hash chain, validator set,
  proposer, entry signatures, finality certificate) and submits a **signed
  receipt** for the range it checked. If the archive serves a block that does
  not verify, the receipt says `invalid` and stops there.
- **Credits you can check.** The balance is derived on the phone from the
  blocks it verified, and compared with what the archive reports. A
  disagreement is shown, not hidden.
- **Traceable and auditable.** Receipts are an append-only signed log on the
  archive; "Export signed receipts" downloads yours.

## Use it

1. The operator publishes the app (Pages workflow, see the monitor README) and
   gives you the archive URL and the **genesis hash** of the cluster.
2. Open the app, enter the archive URL (prefilled if configured), tap *Look
   up cluster*, and **compare the genesis hash** with the one the operator gave
   you. That pins the chain; the archive cannot swap it later.
3. Tap *pair this device*. Credits appear once governance grants them to your
   device ID or a coordinator settles work to it.

The archive must allow the app's origin and the signed-request headers:
`DARM_NODE_CORS_ORIGINS=https://<owner>.github.io`. If it requires a read
token, paste it in the form.

## Run and test

```bash
python3 -m http.server 8090 --bind 127.0.0.1   # in this directory
npm test                                        # node --test, no dependencies
```

The tests check the JavaScript encoder, BLAKE3, Merkle roots, entry and block
verification, wallet math and the engine against the Rust test vectors
(`../spec/test-vectors.json`, section `companion`).

`vendor/noble-hashes` is `@noble/hashes` 2.4.0 (MIT), unmodified, with
checksums in `SHA256SUMS`; no package manager is needed at runtime.

## Limits you should know

- **Ed25519 in the browser** needs iOS/Safari 17+, Chrome 137+ or Firefox 129+.
  Older browsers show a message instead of falling back to a weaker scheme.
- **The key cannot be exported.** Clearing site data or losing the phone loses
  the identity and any credits held by it. That is deliberate: a key you can
  copy is a key someone else can copy.
- **No background work.** Browsers (iOS especially) suspend pages. The app
  verifies while it is open; it cannot run while the phone is locked, and it
  does not read battery or network type on iOS.
- **Scope of compute.** Ledger verification only: no model inference or
  embedding audit on the phone. The archive checks that an `ok` receipt
  matches its own block hash but cannot prove the phone did the work.
- **The archive does not pay.** Credits come from governance grants and
  compute settlements recorded in ledger entries. A receipt is evidence of
  contribution, not a payment.
- **Light client.** It trusts the validator quorum for state rules (spec §11).
- **Store policy.** A web app avoids app-store signing; a native wrapper later
  would need developer accounts, and Apple may reject thin wrappers.
