// DarmRegistry on an in-process EVM (no network). Also checks the ABI decoding
// the join page relies on, using the same decoder against real contract output.
import { test } from "node:test";
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { createRequire } from "node:module";
import { VM } from "@ethereumjs/vm";
import { Common, Chain, Hardfork } from "@ethereumjs/common";
import { LegacyTransaction } from "@ethereumjs/tx";
import { Account, Address, hexToBytes, bytesToHex } from "@ethereumjs/util";
import { Interface, Wallet, keccak256, toUtf8Bytes } from "ethers";
import { encodeNetworkInfoCall, decodeNetworkInfo } from "../../join/chain.js";

const require = createRequire(import.meta.url);
const solc = require("solc");

function compile() {
  const source = readFileSync(new URL("../src/DarmRegistry.sol", import.meta.url), "utf8");
  const input = {
    language: "Solidity",
    sources: { "DarmRegistry.sol": { content: source } },
    settings: { optimizer: { enabled: true, runs: 200 }, outputSelection: { "*": { "*": ["abi", "evm.bytecode.object"] } } },
  };
  const output = JSON.parse(solc.compile(JSON.stringify(input)));
  const problems = (output.errors ?? []).filter((e) => e.severity === "error" || e.severity === "warning");
  assert.equal(problems.length, 0, problems.map((p) => p.formattedMessage).join("\n"));
  const c = output.contracts["DarmRegistry.sol"].DarmRegistry;
  return { abi: c.abi, bytecode: c.evm.bytecode.object };
}

const { abi, bytecode } = compile();
const iface = new Interface(abi);
const common = new Common({ chain: Chain.Mainnet, hardfork: Hardfork.Cancun });
const wallet = (i) => new Wallet(keccak256(toUtf8Bytes(`darm-registry-${i}`)));

class Harness {
  static async create(accounts) {
    const h = new Harness();
    h.vm = await VM.create({ common });
    h.nonces = new Map();
    for (const w of accounts)
      await h.vm.stateManager.putAccount(Address.fromString(w.address), Account.fromAccountData({ balance: 10n ** 20n }));
    return h;
  }
  async send(from, to, data) {
    const nonce = this.nonces.get(from.address) ?? 0n;
    this.nonces.set(from.address, nonce + 1n);
    const tx = LegacyTransaction.fromTxData(
      { nonce, gasPrice: 10n ** 10n, gasLimit: 5_000_000n, to: to ? Address.fromString(to) : undefined, data: hexToBytes(data) },
      { common },
    ).sign(hexToBytes(from.privateKey));
    return this.vm.runTx({ tx, skipBlockGasLimitValidation: true, skipHardForkValidation: true });
  }
  async deploy(from) {
    const r = await this.send(from, undefined, "0x" + bytecode);
    assert.ok(!r.execResult.exceptionError, "deploy failed");
    return r.createdAddress.toString();
  }
  async call(from, to, fn, args) {
    const r = await this.send(from, to, iface.encodeFunctionData(fn, args));
    return { ok: !r.execResult.exceptionError, r };
  }
  async view(to, fn, args) {
    const r = await this.vm.evm.runCall({ to: Address.fromString(to), data: hexToBytes(iface.encodeFunctionData(fn, args)) });
    assert.ok(!r.execResult.exceptionError);
    return iface.decodeFunctionResult(fn, bytesToHex(r.execResult.returnValue));
  }
  revertName(result) {
    const data = bytesToHex(result.r.execResult.returnValue);
    try { return iface.parseError(data)?.name; } catch { return undefined; }
  }
}

const CHAIN = "0x" + "11".repeat(32);
const GENESIS = "0x" + "22".repeat(32);
const NODE = "0x" + "33".repeat(32);

test("register, publish the bootstrap list, rotate it, transfer admin", async () => {
  const [admin, other, next] = [wallet(1), wallet(2), wallet(3)];
  const h = await Harness.create([admin, other, next]);
  const registry = await h.deploy(admin);

  assert.ok((await h.call(admin, registry, "registerNetwork", [CHAIN, GENESIS, ["https://a.example.org", "https://b.example.org"]])).ok);
  const [who, genesis, version, , urls] = await h.view(registry, "networkInfo", [CHAIN]);
  assert.equal(who, admin.address);
  assert.equal(genesis, GENESIS);
  assert.equal(version, 1n);
  assert.deepEqual([...urls], ["https://a.example.org", "https://b.example.org"]);

  assert.equal(h.revertName(await h.call(other, registry, "registerNetwork", [CHAIN, GENESIS, []])), "NetworkExists");
  assert.equal(h.revertName(await h.call(other, registry, "setBootstrap", [CHAIN, ["https://evil.example"]])), "NotAdmin");

  assert.ok((await h.call(admin, registry, "setBootstrap", [CHAIN, ["https://c.example.org"]])).ok);
  const after = await h.view(registry, "networkInfo", [CHAIN]);
  assert.equal(after[2], 2n);
  assert.deepEqual([...after[4]], ["https://c.example.org"]);

  assert.equal(h.revertName(await h.call(next, registry, "acceptAdmin", [CHAIN])), "NotPendingAdmin");
  await h.call(admin, registry, "transferAdmin", [CHAIN, next.address]);
  assert.ok((await h.call(next, registry, "acceptAdmin", [CHAIN])).ok);
  assert.equal((await h.view(registry, "networkInfo", [CHAIN]))[0], next.address);
  assert.equal(h.revertName(await h.call(admin, registry, "setBootstrap", [CHAIN, []])), "NotAdmin");
});

test("limits: url count, empty and oversized urls, zero values, unknown network", async () => {
  const admin = wallet(1);
  const h = await Harness.create([admin]);
  const registry = await h.deploy(admin);
  const nine = Array.from({ length: 9 }, (_, i) => `https://n${i}.example`);
  assert.equal(h.revertName(await h.call(admin, registry, "registerNetwork", [CHAIN, GENESIS, nine])), "TooManyUrls");
  assert.equal(h.revertName(await h.call(admin, registry, "registerNetwork", [CHAIN, GENESIS, [""]])), "BadUrl");
  assert.equal(h.revertName(await h.call(admin, registry, "registerNetwork", [CHAIN, GENESIS, ["https://" + "a".repeat(130)]])), "BadUrl");
  assert.equal(h.revertName(await h.call(admin, registry, "registerNetwork", ["0x" + "00".repeat(32), GENESIS, []])), "ZeroValue");
  assert.equal(h.revertName(await h.call(admin, registry, "claimNode", [CHAIN, NODE, "0x" + "ab".repeat(64)])), "UnknownNetwork");
  assert.equal((await h.call(admin, registry, "networkInfo", [CHAIN])).ok, false);
});

test("node claims are recorded with their signature and bounded", async () => {
  const [admin, user] = [wallet(1), wallet(2)];
  const h = await Harness.create([admin, user]);
  const registry = await h.deploy(admin);
  await h.call(admin, registry, "registerNetwork", [CHAIN, GENESIS, []]);
  const sig = "0x" + "ab".repeat(64);
  const { ok, r } = await h.call(user, registry, "claimNode", [CHAIN, NODE, sig]);
  assert.ok(ok);
  const log = r.execResult.logs[0];
  const parsed = iface.parseLog({ topics: log[1].map((t) => bytesToHex(t)), data: bytesToHex(log[2]) });
  assert.equal(parsed.name, "NodeClaimed");
  assert.equal(parsed.args.wallet, user.address);
  assert.equal(parsed.args.nodeId, NODE);
  assert.equal(parsed.args.signature, sig);
  assert.equal((await h.view(registry, "claimedNodeOf", [CHAIN, user.address]))[0], NODE);
  assert.equal(h.revertName(await h.call(user, registry, "claimNode", [CHAIN, NODE, "0x"])), "BadSignature");
  assert.equal(h.revertName(await h.call(user, registry, "claimNode", [CHAIN, NODE, "0x" + "ab".repeat(129)])), "BadSignature");
});

test("the join page's raw ABI codec matches the contract", async () => {
  const admin = wallet(1);
  const h = await Harness.create([admin]);
  const registry = await h.deploy(admin);
  await h.call(admin, registry, "registerNetwork", [CHAIN, GENESIS, ["https://a.example.org", "https://bb.example.org/x"]]);
  assert.equal(encodeNetworkInfoCall(CHAIN), iface.encodeFunctionData("networkInfo", [CHAIN]));
  const raw = await h.vm.evm.runCall({ to: Address.fromString(registry), data: hexToBytes(encodeNetworkInfoCall(CHAIN)) });
  const decoded = decodeNetworkInfo(bytesToHex(raw.execResult.returnValue));
  assert.equal(decoded.admin, admin.address.toLowerCase());
  assert.equal(decoded.genesis, GENESIS.slice(2));
  assert.equal(decoded.version, 1);
  assert.deepEqual(decoded.urls, ["https://a.example.org", "https://bb.example.org/x"]);
  assert.throws(() => decodeNetworkInfo("0x1234"));
});
