import assert from "node:assert/strict";
import test from "node:test";

import { compareVersions, readReleaseConfig } from "./release-config.mjs";

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
