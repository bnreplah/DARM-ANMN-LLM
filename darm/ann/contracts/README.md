# DARM-ANN contracts

| Contract | Purpose |
|---|---|
| `DarmAnchor` | Anchors finalized ledger checkpoints, approved by more than 2/3 of the validators' EVM accounts. |
| `DarmRegistry` | On-chain discovery: a network's archive URL list (the decentralized pivot) and wallet-to-node-id claims. |

Both are small, hold no ether and make no external calls. Spec section 12 has
the registry's trust model: **pin the admin address**, because registration is
first come, first served.

```bash
npm ci --ignore-scripts
npm test                                    # in-process EVM, no network
node scripts/deploy.mjs --network sepolia --dry-run
```

## Deploy to a test network

Supported: `sepolia`, `base-sepolia`, `arbitrum-sepolia`. Mainnets are not
selectable.

```bash
export DEPLOYER_KEY=0x...    # a throwaway key funded with test ETH
node scripts/deploy.mjs --network sepolia \
  --darm-chain <32-byte chain id from GET /v1/info> \
  --genesis <genesis hash from GET /v1/info> \
  --urls https://archive.example.org
```

Or run the **Deploy DARM contracts (testnet)** workflow (manual; uses the
`testnet-deploy` environment and its `DEPLOYER_KEY` secret, so add required
reviewers there).

The script prints a `config.json` snippet. Put the `onchain` array in the
repository variable `DARM_ONCHAIN_REGISTRIES` and re-run the Pages workflow:
the join page then finds archives from the chain when the site's own list is
empty or down, checking the admin and genesis you pinned.

Anyone can read the registry; only the admin can change the list. To publish
a new archive later, call `setBootstrap` from the admin wallet.
