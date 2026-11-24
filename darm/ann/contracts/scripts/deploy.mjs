// Deploy DarmAnchor and DarmRegistry to an EVM test network, and optionally
// register a DARM network in the registry.
//
//   node scripts/deploy.mjs --network sepolia --dry-run
//   DEPLOYER_KEY=0x... node scripts/deploy.mjs --network base-sepolia \
//       --darm-chain <32-byte hex chain id> --genesis <32-byte hex> \
//       --urls https://archive.example.org,https://archive2.example.org
//
// The key is read from DEPLOYER_KEY only, never from an argument, and is used
// only to sign locally. Use a throwaway key funded with test ETH. The script
// refuses to run if the RPC reports a different chain id than the network
// name implies, and it refuses mainnet chain ids altogether.
import { readFileSync } from 'node:fs';
import { createRequire } from 'node:module';
import { Contract, ContractFactory, JsonRpcProvider, Wallet, getCreateAddress, formatEther } from 'ethers';

const require = createRequire(import.meta.url);
const solc = require('solc');

export const NETWORKS = {
  sepolia: { chainId: 11155111n, rpc: 'https://ethereum-sepolia-rpc.publicnode.com', explorer: 'https://sepolia.etherscan.io' },
  'base-sepolia': { chainId: 84532n, rpc: 'https://sepolia.base.org', explorer: 'https://sepolia.basescan.org' },
  'arbitrum-sepolia': { chainId: 421614n, rpc: 'https://sepolia-rollup.arbitrum.io/rpc', explorer: 'https://sepolia.arbiscan.io' },
};
const HEX32 = /^0x[0-9a-fA-F]{64}$/;

export function compile(name) {
  const source = readFileSync(new URL(`../src/${name}.sol`, import.meta.url), 'utf8');
  const out = JSON.parse(solc.compile(JSON.stringify({
    language: 'Solidity',
    sources: { [`${name}.sol`]: { content: source } },
    settings: { optimizer: { enabled: true, runs: 200 }, outputSelection: { '*': { '*': ['abi', 'evm.bytecode.object'] } } },
  })));
  const errors = (out.errors ?? []).filter((e) => e.severity === 'error');
  if (errors.length) throw new Error(errors.map((e) => e.formattedMessage).join('\n'));
  const c = out.contracts[`${name}.sol`][name];
  return { abi: c.abi, bytecode: '0x' + c.evm.bytecode.object };
}

export function parseArgs(argv, env = {}) {
  const args = { dryRun: false };
  for (let i = 0; i < argv.length; i++) {
    const a = argv[i];
    if (a === '--dry-run') args.dryRun = true;
    else if (['--network', '--darm-chain', '--genesis', '--urls', '--rpc'].includes(a)) args[a.slice(2).replace(/-(\w)/g, (_, c) => c.toUpperCase())] = argv[++i];
    else throw new Error(`unknown argument ${a}`);
  }
  const net = NETWORKS[args.network];
  if (!net) throw new Error(`--network must be one of ${Object.keys(NETWORKS).join(', ')}`);
  if (args.rpc && !/^https:\/\//.test(args.rpc)) throw new Error('--rpc must be https');
  if ((args.darmChain || args.genesis || args.urls) && !(HEX32.test(args.darmChain ?? '') && HEX32.test(args.genesis ?? '')))
    throw new Error('--darm-chain and --genesis must both be 32-byte hex to register a network');
  const urls = args.urls ? args.urls.split(',').map((u) => u.trim()).filter(Boolean) : [];
  for (const u of urls) if (!/^https:\/\/[^\s]+$/.test(u)) throw new Error(`url must be https: ${u}`);
  if (!args.dryRun && !/^0x[0-9a-fA-F]{64}$/.test(env.DEPLOYER_KEY ?? '')) throw new Error('set DEPLOYER_KEY (0x + 64 hex) in the environment');
  return { ...args, urls, net, rpc: args.rpc ?? env.DARM_RPC_URL ?? net.rpc };
}

async function main() {
  const args = parseArgs(process.argv.slice(2), process.env);
  const anchor = compile('DarmAnchor');
  const registry = compile('DarmRegistry');
  console.log(`network: ${args.network} (chain id ${args.net.chainId}), rpc: ${new URL(args.rpc).host}`);
  console.log(`DarmAnchor bytecode ${(anchor.bytecode.length - 2) / 2} bytes, DarmRegistry ${(registry.bytecode.length - 2) / 2} bytes`);
  if (args.dryRun) {
    console.log('dry run: nothing sent');
    return;
  }
  const provider = new JsonRpcProvider(args.rpc);
  const reported = (await provider.getNetwork()).chainId;
  if (reported !== args.net.chainId) throw new Error(`RPC reports chain id ${reported}, expected ${args.net.chainId}; refusing to send`);
  const signer = new Wallet(process.env.DEPLOYER_KEY, provider);
  console.log(`deployer ${signer.address}, balance ${formatEther(await provider.getBalance(signer.address))} ETH`);
  const nonce = await provider.getTransactionCount(signer.address);
  console.log(`expected addresses: anchor ${getCreateAddress({ from: signer.address, nonce })}, registry ${getCreateAddress({ from: signer.address, nonce: nonce + 1 })}`);

  const deployed = {};
  for (const [name, c] of [['DarmAnchor', anchor], ['DarmRegistry', registry]]) {
    const contract = await new ContractFactory(c.abi, c.bytecode, signer).deploy();
    await contract.waitForDeployment();
    deployed[name] = await contract.getAddress();
    console.log(`${name}: ${deployed[name]}  ${args.net.explorer}/address/${deployed[name]}`);
  }
  if (args.darmChain) {
    const reg = new Contract(deployed.DarmRegistry, registry.abi, signer);
    const tx = await reg.registerNetwork(args.darmChain, args.genesis, args.urls);
    await tx.wait();
    console.log(`registered DARM chain ${args.darmChain} with ${args.urls.length} archive url(s): ${args.net.explorer}/tx/${tx.hash}`);
  }
  console.log('\nAdd to the site config (config.json) so the join page can find it:');
  console.log(JSON.stringify({ onchain: [{ name: args.network, rpc: args.rpc, registry: deployed.DarmRegistry, chainId: args.darmChain ?? '<darm chain id>', admin: signer.address, genesis: args.genesis ?? undefined }] }, null, 2));
}

if (import.meta.url === `file://${process.argv[1]}`) main().catch((e) => { console.error(e.message); process.exit(1); });
