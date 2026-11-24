// Tests for DarmAnchor on an in-process EVM: compile with solc-js, deploy and
// call through @ethereumjs/vm. No RPC node or network access is needed.
import { test } from "node:test";
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { createRequire } from "node:module";
import { VM } from "@ethereumjs/vm";
import { Common, Chain, Hardfork } from "@ethereumjs/common";
import { LegacyTransaction } from "@ethereumjs/tx";
import { Account, Address, hexToBytes, bytesToHex } from "@ethereumjs/util";
import { Interface, Wallet, keccak256, toUtf8Bytes } from "ethers";

const require = createRequire(import.meta.url);
const solc = require("solc");

function compile() {
  const source = readFileSync(new URL("../src/DarmAnchor.sol", import.meta.url), "utf8");
  const input = {
    language: "Solidity",
    sources: { "DarmAnchor.sol": { content: source } },
    settings: { optimizer: { enabled: true, runs: 200 }, outputSelection: { "*": { "*": ["abi", "evm.bytecode.object"] } } },
  };
  const output = JSON.parse(solc.compile(JSON.stringify(input)));
  const errors = (output.errors ?? []).filter((e) => e.severity === "error");
  assert.equal(errors.length, 0, JSON.stringify(errors, null, 2));
  const warnings = (output.errors ?? []).filter((e) => e.severity === "warning");
  assert.equal(warnings.length, 0, `compiler warnings: ${warnings.map((w) => w.formattedMessage).join("\n")}`);
  const contract = output.contracts["DarmAnchor.sol"].DarmAnchor;
  return { abi: contract.abi, bytecode: contract.evm.bytecode.object };
}

const { abi, bytecode } = compile();
const iface = new Interface(abi);
const common = new Common({ chain: Chain.Mainnet, hardfork: Hardfork.Cancun });

function wallet(i) {
  return new Wallet(keccak256(toUtf8Bytes(`darm-test-${i}`)));
}

class Harness {
  static async create(accounts) {
    const h = new Harness();
    h.vm = await VM.create({ common });
    h.nonces = new Map();
    for (const w of accounts) {
      await h.vm.stateManager.putAccount(Address.fromString(w.address), Account.fromAccountData({ balance: 10n ** 20n }));
    }
    return h;
  }

  async send(from, to, data) {
    const nonce = this.nonces.get(from.address) ?? 0n;
    this.nonces.set(from.address, nonce + 1n);
    const tx = LegacyTransaction.fromTxData(
      { nonce, gasPrice: 10n ** 10n, gasLimit: 5_000_000n, to: to ? Address.fromString(to) : undefined, data: hexToBytes(data) },
      { common },
    ).sign(hexToBytes(from.privateKey));
    const result = await this.vm.runTx({ tx, skipBlockGasLimitValidation: true, skipHardForkValidation: true });
    return result;
  }

  async deploy(from) {
    const result = await this.send(from, undefined, "0x" + bytecode);
    assert.equal(result.execResult.exceptionError, undefined);
    this.address = result.createdAddress.toString();
  }

  async call(from, fn, args) {
    const result = await this.send(from, this.address, iface.encodeFunctionData(fn, args));
    const ret = bytesToHex(result.execResult.returnValue);
    if (result.execResult.exceptionError) {
      const error = ret.length > 2 ? iface.parseError(ret)?.name : "revert";
      return { error, logs: [] };
    }
    const logs = result.receipt.logs.map(([, topics, data]) =>
      iface.parseLog({ topics: topics.map(bytesToHex), data: bytesToHex(data) }),
    );
    return { value: iface.decodeFunctionResult(fn, ret), logs };
  }

  async view(fn, args) {
    const result = await this.vm.evm.runCall({ to: Address.fromString(this.address), data: hexToBytes(iface.encodeFunctionData(fn, args)) });
    return iface.decodeFunctionResult(fn, bytesToHex(result.execResult.returnValue));
  }
}

const CHAIN = keccak256(toUtf8Bytes("darm-chain"));
const H = (s) => keccak256(toUtf8Bytes(s));

async function setup(n = 4) {
  const admin = wallet("admin");
  const vals = Array.from({ length: n }, (_, i) => wallet(i));
  const outsider = wallet("outsider");
  const h = await Harness.create([admin, outsider, ...vals]);
  await h.deploy(admin);
  const r = await h.call(admin, "registerChain", [CHAIN, vals.map((v) => v.address), vals.map((_, i) => H(`node${i}`))]);
  assert.equal(r.error, undefined);
  return { h, admin, vals, outsider };
}

test("anchors only after strictly more than two thirds approve identical values", async () => {
  const { h, vals } = await setup(4);
  const args = [CHAIN, 1, H("block1"), H("root1")];
  assert.equal((await h.call(vals[0], "approveCheckpoint", args)).value[0], false);
  assert.equal((await h.call(vals[1], "approveCheckpoint", args)).value[0], false);
  // A conflicting value at the same height does not count toward the first.
  assert.equal((await h.call(vals[2], "approveCheckpoint", [CHAIN, 1, H("evil"), H("root1")])).value[0], false);
  const final = await h.call(vals[3], "approveCheckpoint", args);
  assert.equal(final.value[0], true);
  assert.ok(final.logs.some((l) => l.name === "CheckpointAnchored"));
  assert.equal((await h.view("isAnchored", [CHAIN, 1, H("block1")]))[0], true);
  assert.equal((await h.view("isAnchored", [CHAIN, 1, H("evil")]))[0], false);
  // Older or equal heights are stale now.
  assert.equal((await h.call(vals[2], "approveCheckpoint", args)).error, "StaleHeight");
});

test("rejects outsiders, double votes and zero values", async () => {
  const { h, vals, outsider } = await setup(3);
  const args = [CHAIN, 7, H("b"), H("r")];
  assert.equal((await h.call(outsider, "approveCheckpoint", args)).error, "NotValidator");
  assert.equal((await h.call(vals[0], "approveCheckpoint", args)).error, undefined);
  assert.equal((await h.call(vals[0], "approveCheckpoint", args)).error, "AlreadyApproved");
  assert.equal((await h.call(vals[0], "approveCheckpoint", [CHAIN, 8, "0x" + "00".repeat(32), H("r")])).error, "ZeroValue");
  assert.equal((await h.call(vals[0], "approveCheckpoint", [H("nope"), 1, H("b"), H("r")])).error, "UnknownChain");
});

test("validator set changes are admin-only and void old approvals", async () => {
  const { h, admin, vals, outsider } = await setup(3);
  const args = [CHAIN, 1, H("b"), H("r")];
  await h.call(vals[0], "approveCheckpoint", args);
  await h.call(vals[1], "approveCheckpoint", args);
  assert.equal(
    (await h.call(outsider, "setValidators", [CHAIN, [outsider.address], [H("x")]])).error,
    "NotAdmin",
  );
  const next = [vals[0], vals[1], outsider];
  assert.equal((await h.call(admin, "setValidators", [CHAIN, next.map((v) => v.address), next.map((_, i) => H(`n${i}`))])).error, undefined);
  // vals[2] was removed; the two old approvals no longer count in epoch 2.
  assert.equal((await h.call(vals[2], "approveCheckpoint", args)).error, "NotValidator");
  assert.equal((await h.call(outsider, "approveCheckpoint", args)).value[0], false);
  assert.equal((await h.call(vals[0], "approveCheckpoint", args)).value[0], false);
  assert.equal((await h.call(vals[1], "approveCheckpoint", args)).value[0], true);
});

test("validator set validation and two-step admin transfer", async () => {
  const { h, admin, vals, outsider } = await setup(2);
  const dup = [vals[0].address, vals[0].address];
  assert.equal((await h.call(admin, "setValidators", [CHAIN, dup, [H("a"), H("b")]])).error, "InvalidValidatorSet");
  assert.equal((await h.call(admin, "setValidators", [CHAIN, [], []])).error, "InvalidValidatorSet");
  assert.equal((await h.call(admin, "setValidators", [CHAIN, [vals[0].address], []])).error, "InvalidValidatorSet");
  assert.equal((await h.call(outsider, "registerChain", [CHAIN, [outsider.address], [H("o")]])).error, "ChainExists");

  assert.equal((await h.call(admin, "transferAdmin", [CHAIN, outsider.address])).error, undefined);
  assert.equal((await h.call(vals[0], "acceptAdmin", [CHAIN])).error, "NotPendingAdmin");
  assert.equal((await h.call(outsider, "acceptAdmin", [CHAIN])).error, undefined);
  assert.equal((await h.view("chainInfo", [CHAIN]))[0], outsider.address);
  assert.equal((await h.call(admin, "transferAdmin", [CHAIN, admin.address])).error, "NotAdmin");
});
