import assert from "node:assert/strict";
import { createHash } from "node:crypto";
import test from "node:test";

import {
  affectsNativeRelease,
  nextVersion,
  parseArguments,
  releaseCommitMessage,
  secretsReadByServer,
  verifyManifest,
  withVersion,
} from "./deploy-prod.mjs";

test("options default to a patch release that deploys the server", () => {
  assert.deepEqual(parseArguments([]), {
    dryRun: false,
    yes: false,
    message: undefined,
    version: undefined,
    bump: "patch",
    release: "auto",
    server: true,
    verifyOnly: false,
  });
  const options = parseArguments(["--minor", "--message", "chat fixes", "--no-release", "-y"]);
  assert.equal(options.bump, "minor");
  assert.equal(options.message, "chat fixes");
  assert.equal(options.release, "never");
  assert.equal(options.yes, true);
});

test("invalid arguments are refused", () => {
  assert.throws(() => parseArguments(["--prod"]), /Unknown argument/);
  assert.throws(() => parseArguments(["--message"]), /needs a value/);
  assert.throws(() => parseArguments(["--version", "next"]), /invalid semantic version/);
  assert.throws(() => parseArguments(["--skip-server", "--no-release"]), /nothing to deploy/);
  assert.deepEqual(parseArguments(["--help"]), { help: true });
});

test("the next version is newer than both release.json and the live manifest", () => {
  assert.equal(nextVersion({ configured: "0.3.2", published: "0.3.2", bump: "patch" }), "0.3.3");
  assert.equal(nextVersion({ configured: "0.3.2", published: "0.3.2", bump: "minor" }), "0.4.0");
  assert.equal(nextVersion({ configured: "0.3.2", published: "0.3.2", bump: "major" }), "1.0.0");
  // A failed release left release.json ahead of production.
  assert.equal(nextVersion({ configured: "0.3.3", published: "0.3.2", bump: "patch" }), "0.3.4");
  assert.equal(nextVersion({ configured: "0.3.2", published: "0.3.5", bump: "patch" }), "0.3.6");
  assert.equal(nextVersion({ configured: "0.3.2", published: "0.3.2", requested: "0.5.0" }), "0.5.0");
  assert.throws(() => nextVersion({ configured: "0.3.2", published: "0.3.2", requested: "0.3.2" }), /greater/);
});

test("only files outside the native release skip it", () => {
  for (const path of ["server/src/lib.rs", "docs/native-releases.md", "README.md", "agent/README.md",
    "scripts/tests/test_transport_security.py", "scripts/deploy-prod.test.mjs", ".github/workflows/ci.yml"]) {
    assert.equal(affectsNativeRelease(path), false, path);
  }
  for (const path of ["agent/src/main.rs", "remote/src/lib.rs", "crates/chat/src/lib.rs",
    "dashboard/app/page.tsx", "Cargo.lock", "scripts/build-agent.ps1",
    ".github/workflows/native-release-build.yml"]) {
    assert.equal(affectsNativeRelease(path), true, path);
  }
});

test("secrets are collected from the server's env.secret calls", () => {
  assert.deepEqual(
    secretsReadByServer(['env.secret("WORKOS_API_KEY")?', 'env.secret("TURN_KEY_ID") env.var("PUBLIC_API_URL")',
      'env.secret("TURN_KEY_ID")']),
    ["TURN_KEY_ID", "WORKOS_API_KEY"],
  );
});

test("the release commit changes only the version", () => {
  const config = '{\n  "version": "0.3.2",\n  "download_origin": "https://meshrmm.com"\n}\n';
  assert.equal(withVersion(config, "0.3.3"), config.replace("0.3.2", "0.3.3"));
  assert.throws(() => withVersion("{}", "0.3.3"), /no version/);
  assert.equal(releaseCommitMessage("0.3.3", "chat fixes"), "chore(release): publish chat fixes 0.3.3");
  assert.equal(releaseCommitMessage("0.3.3"), "chore(release): publish 0.3.3");
});

test("the manifest check compares versions and downloaded checksums", async () => {
  const body = Buffer.from("agent");
  const sha256 = createHash("sha256").update(body).digest("hex");
  const manifest = {
    schema_version: 1,
    releases: {
      "agent-windows-x64": { version: "0.3.3", url: "https://meshrmm.com/downloads/a.exe", sha256 },
      "client-windows-x64": { version: "0.3.3", url: "https://meshrmm.com/downloads/b.exe", sha256: "0".repeat(64) },
      "client-macos-arm64": { version: "0.3.2", url: "https://meshrmm.com/downloads/c.zip", sha256 },
    },
  };
  const requested = [];
  const fetch = async (url) => {
    requested.push(new URL(url).pathname);
    return new Response(body);
  };
  assert.deepEqual(await verifyManifest(manifest, "0.3.3", { fetch }), [
    "client-windows-x64 download does not match its manifest SHA-256",
    "client-macos-arm64 is 0.3.2, expected 0.3.3",
  ]);
  assert.deepEqual(requested, ["/downloads/a.exe", "/downloads/b.exe"]);
  assert.deepEqual(await verifyManifest({ schema_version: 1, releases: {} }, "0.3.3", { fetch }), [
    "the update manifest has no releases",
  ]);
});
