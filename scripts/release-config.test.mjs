import assert from "node:assert/strict";
import test from "node:test";

import { assertNotOlder, compareVersions, readReleaseConfig } from "./release-config.mjs";

test("the checked-in release configuration is valid", async () => {
  const config = await readReleaseConfig({});
  assert.match(config.version, /^\d+\.\d+\.\d+/);
  assert.match(config.publicKey, /^[0-9a-f]{64}$/);
});

test("a development key replaces the release key", async () => {
  const key = "AB".repeat(32);
  assert.equal((await readReleaseConfig({ MESHRMM_RELEASE_PUBLIC_KEY: key })).publicKey, key.toLowerCase());
  await assert.rejects(readReleaseConfig({ MESHRMM_RELEASE_PUBLIC_KEY: "abc" }), /32 bytes/);
});

test("semantic release ordering handles stable and prerelease versions", () => {
  assert.equal(compareVersions("0.2.1", "0.2.0"), 1);
  assert.equal(compareVersions("0.3.0", "0.2.99"), 1);
  assert.equal(compareVersions("1.0.0", "1.0.0"), 0);
  assert.equal(compareVersions("1.0.0-beta.2", "1.0.0-beta.1"), 1);
  assert.equal(compareVersions("1.0.0", "1.0.0-rc.1"), 1);
  assert.equal(compareVersions("1.0.0-alpha", "1.0.0-alpha.1"), -1);
});

test("a republish must not be older than the published release", () => {
  assertNotOlder("0.2.8", "0.2.8");
  assertNotOlder("0.3.0", "0.2.8");
  assert.throws(() => assertNotOlder("0.2.7", "0.2.8"), /older than the published 0.2.8/);
  assert.throws(() => assertNotOlder("0.2.8-rc.1", "0.2.8"), /older/);
});
