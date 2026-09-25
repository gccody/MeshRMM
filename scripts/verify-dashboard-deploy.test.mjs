import assert from "node:assert/strict";
import { createHash } from "node:crypto";
import { mkdtemp, rm, unlink, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import test from "node:test";

import { readReleaseConfig } from "./release-config.mjs";
import { verifyDashboardDeploy } from "./verify-dashboard-deploy.mjs";

const release = { MESHRMM_RELEASE_DEPLOY: "1" };
const artifacts = {
  "agent-windows-x64": "meshrmm-agent-windows-x64.exe",
  "client-windows-x64": "meshrmm-remote-windows-x64.exe",
  "client-macos-arm64": "meshrmm-remote-macos-arm64.zip",
};

// Writes a complete release like the one the release workflow assembles.
async function assemble(t, { version } = {}) {
  const config = await readReleaseConfig();
  const directory = await mkdtemp(join(tmpdir(), "meshrmm-downloads-"));
  t.after(() => rm(directory, { recursive: true, force: true }));
  const manifest = { schema_version: 1, releases: {} };
  for (const [target, filename] of Object.entries(artifacts)) {
    const contents = `${target} build`;
    const sha256 = createHash("sha256").update(contents).digest("hex");
    await writeFile(join(directory, filename), contents);
    if (filename.endsWith(".exe")) await writeFile(join(directory, `${filename}.sha256`), `${sha256}\n`);
    manifest.releases[target] = {
      version: version ?? config.version,
      url: `${config.downloadOrigin}/downloads/${filename}`,
      sha256,
    };
  }
  const manifestPath = join(directory, "update-manifest.json");
  await writeFile(manifestPath, JSON.stringify(manifest));
  return { directory, manifestPath, manifest };
}

test("a complete release deploys from the release workflow", async t => {
  const { manifestPath } = await assemble(t);
  const { version } = await readReleaseConfig();
  assert.equal(await verifyDashboardDeploy({ environment: release, manifestPath }), version);
});

test("deploying outside the release workflow is refused", async t => {
  const { manifestPath } = await assemble(t);
  await assert.rejects(
    verifyDashboardDeploy({ environment: {}, manifestPath }),
    /Publish native release workflow/,
  );
});

test("a checkout without downloads is refused", async t => {
  const { directory } = await assemble(t);
  await assert.rejects(
    verifyDashboardDeploy({ environment: release, manifestPath: join(directory, "missing", "update-manifest.json") }),
    /Refusing to deploy without the complete native release/,
  );
});

test("a missing artifact is refused", async t => {
  const { directory, manifestPath } = await assemble(t);
  await unlink(join(directory, "meshrmm-remote-macos-arm64.zip"));
  await assert.rejects(verifyDashboardDeploy({ environment: release, manifestPath }), /ENOENT/);
});

test("artifacts for another version are refused", async t => {
  const { manifestPath } = await assemble(t, { version: "0.0.1" });
  await assert.rejects(verifyDashboardDeploy({ environment: release, manifestPath }), /does not match/);
});

test("a replaced artifact is refused", async t => {
  const { directory, manifestPath } = await assemble(t);
  await writeFile(join(directory, "meshrmm-agent-windows-x64.exe"), "local build");
  await assert.rejects(verifyDashboardDeploy({ environment: release, manifestPath }), /SHA-256 does not match/);
});

test("a download URL on another origin is refused", async t => {
  const { manifestPath, manifest } = await assemble(t);
  manifest.releases["client-windows-x64"].url = "https://example.com/downloads/meshrmm-remote-windows-x64.exe";
  await writeFile(manifestPath, JSON.stringify(manifest));
  await assert.rejects(verifyDashboardDeploy({ environment: release, manifestPath }), /URL does not point/);
});

test("unexpected files in the downloads are refused", async t => {
  const { directory, manifestPath } = await assemble(t);
  await writeFile(join(directory, "meshrmm-agent-windows-x64-debug.exe"), "debug build");
  await assert.rejects(verifyDashboardDeploy({ environment: release, manifestPath }), /unexpected files/);
});
