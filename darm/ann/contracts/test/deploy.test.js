import { test } from "node:test";
import assert from "node:assert/strict";
import { parseArgs, compile, NETWORKS } from "../scripts/deploy.mjs";

const KEY = "0x" + "11".repeat(32);

test("only the three testnets are selectable and mainnet is not", () => {
  assert.deepEqual(Object.keys(NETWORKS).sort(), ["arbitrum-sepolia", "base-sepolia", "sepolia"]);
  assert.throws(() => parseArgs(["--network", "mainnet", "--dry-run"]), /must be one of/);
  for (const n of Object.values(NETWORKS)) assert.ok(![1n, 8453n, 42161n, 137n].includes(n.chainId));
});

test("a key is required to send, and only from the environment", () => {
  assert.throws(() => parseArgs(["--network", "sepolia"], {}), /DEPLOYER_KEY/);
  assert.throws(() => parseArgs(["--network", "sepolia", "--key", KEY], { DEPLOYER_KEY: KEY }), /unknown argument/);
  assert.equal(parseArgs(["--network", "sepolia"], { DEPLOYER_KEY: KEY }).dryRun, false);
  assert.equal(parseArgs(["--network", "sepolia", "--dry-run"], {}).dryRun, true);
});

test("registration arguments are validated", () => {
  const ok = ["--network", "base-sepolia", "--dry-run", "--darm-chain", "0x" + "11".repeat(32), "--genesis", "0x" + "22".repeat(32), "--urls", "https://a.example.org, https://b.example.org"];
  assert.deepEqual(parseArgs(ok, {}).urls, ["https://a.example.org", "https://b.example.org"]);
  assert.throws(() => parseArgs([...ok.slice(0, 4), "0xzz", ...ok.slice(5)], {}), /32-byte hex/);
  assert.throws(() => parseArgs([...ok.slice(0, -1), "http://a.example.org"], {}), /must be https/);
  assert.throws(() => parseArgs(["--network", "sepolia", "--dry-run", "--rpc", "http://x"], {}), /must be https/);
});

test("both contracts compile for deployment", () => {
  for (const name of ["DarmAnchor", "DarmRegistry"]) assert.ok(compile(name).bytecode.length > 1000);
});
