import assert from "node:assert/strict";
import test from "node:test";

import {
  assertNotOlderThanManifest,
  compareVersions,
  readReleaseConfig,
} from "./release-config.mjs";

test("the checked-in release configuration is valid", async () => {
  const config = await readReleaseConfig();
  assert.match(config.version, /^\d+\.\d+\.\d+/);
  assert.equal(new URL(config.downloadOrigin).protocol, "https:");
  assert.equal(new URL(config.viewerServer).protocol, "https:");
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
  const manifest = {
    schema_version: 1,
    releases: {
      "agent-windows-x64": { version: "0.2.8" },
      "client-macos-arm64": { version: "0.2.7" },
    },
  };
  assertNotOlderThanManifest("0.2.8", manifest);
  assertNotOlderThanManifest("0.3.0", manifest);
  assert.throws(
    () => assertNotOlderThanManifest("0.2.7", manifest),
    /older than the published agent-windows-x64 0.2.8/,
  );
  assert.throws(() => assertNotOlderThanManifest("0.2.8-rc.1", manifest), /older/);
  assert.throws(
    () => assertNotOlderThanManifest("0.2.8", { schema_version: 1, releases: {} }),
    /no releases/,
  );
  assert.throws(
    () => assertNotOlderThanManifest("0.2.8", { schema_version: 2, releases: manifest.releases }),
    /no releases/,
  );
  assert.throws(
    () =>
      assertNotOlderThanManifest("0.2.8", {
        schema_version: 1,
        releases: { "agent-windows-x64": {} },
      }),
    /no version for agent-windows-x64/,
  );
});
