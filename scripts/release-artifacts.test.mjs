import assert from "node:assert/strict";
import { createHash, createPrivateKey, generateKeyPairSync, sign } from "node:crypto";
import { mkdtemp, readFile, rm, unlink, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import test from "node:test";

import {
  ARTIFACTS,
  MANIFEST,
  publicKeyHex,
  signedMessage,
  verifyArtifacts,
  writeArtifacts,
} from "./release-artifacts.mjs";

const version = "1.2.0";
const { privateKey } = generateKeyPairSync("ed25519");
const signingKey = privateKey.export({ type: "pkcs8", format: "pem" });
const publicKey = publicKeyHex(privateKey);

// A directory with a build for every target, or for `targets`.
async function builds(t, targets = Object.keys(ARTIFACTS)) {
  const directory = await mkdtemp(join(tmpdir(), "meshrmm-artifacts-"));
  t.after(() => rm(directory, { recursive: true, force: true }));
  for (const target of targets) await writeFile(join(directory, ARTIFACTS[target]), `${target} build`);
  return directory;
}

test("signatures match the Rust test vector", () => {
  // crates/self-update's tests verify this signature with the same key.
  const key = createPrivateKey({
    key: Buffer.concat([Buffer.from("302e020100300506032b657004220420", "hex"), Buffer.alloc(32, 7)]),
    format: "der",
    type: "pkcs8",
  });
  assert.equal(publicKeyHex(key), "ea4a6c63e29c520abef5507b132ec5f9954776aebebe7b92421eea691446d22c");
  const digest = createHash("sha256").update("agent").digest("hex");
  assert.equal(
    sign(null, Buffer.from(signedMessage("agent-windows-x64", "1.2.0", digest)), key).toString("hex"),
    "dfa675701363af7e76af23799742517bed7c0ad6f8f7f572c712b0e8e7d2e3cf16d9c9ff8ec73879fb2da3a2c08403a8f4e987fc21b07f8a148c2a56dd61b80f",
  );
});

test("a signed, complete release verifies", async t => {
  const directory = await builds(t);
  const manifest = await writeArtifacts(directory, { version, publicKey, signingKey });
  assert.equal(manifest.version, version);
  assert.deepEqual(Object.keys(manifest.artifacts).sort(), Object.keys(ARTIFACTS).sort());
  await verifyArtifacts(directory, { version, publicKey });
});

test("a signing key must match the release key", async t => {
  const directory = await builds(t);
  const other = generateKeyPairSync("ed25519").privateKey.export({ type: "pkcs8", format: "pem" });
  await assert.rejects(writeArtifacts(directory, { version, publicKey, signingKey: other }), /not the key/);
});

test("unsigned builds verify only when allowed", async t => {
  const directory = await builds(t);
  await writeArtifacts(directory, { version, publicKey });
  await assert.rejects(verifyArtifacts(directory, { version, publicKey }), /not signed/);
  await verifyArtifacts(directory, { version, publicKey, allowUnsigned: true });
});

test("a local build describes only what it built", async t => {
  const directory = await builds(t, ["agent-windows-x64"]);
  const manifest = await writeArtifacts(directory, { version, publicKey });
  assert.deepEqual(Object.keys(manifest.artifacts), ["agent-windows-x64"]);
  await assert.rejects(verifyArtifacts(directory, { version, publicKey, allowUnsigned: true }), /missing/);
  await assert.rejects(writeArtifacts(await builds(t, []), { version, publicKey }), /contains none/);
});

test("verification refuses tampered, mislabeled or extra files", async t => {
  const directory = await builds(t);
  await writeArtifacts(directory, { version, publicKey, signingKey });
  await assert.rejects(verifyArtifacts(directory, { version: "1.2.1", publicKey }), /does not match/);

  await writeFile(join(directory, "notes.txt"), "");
  await assert.rejects(verifyArtifacts(directory, { version, publicKey }), /unexpected files: notes.txt/);
  await unlink(join(directory, "notes.txt"));

  const manifest = JSON.parse(await readFile(join(directory, MANIFEST), "utf8"));
  // Re-signing a build under another version doesn't carry its signature.
  manifest.version = "9.9.9";
  await writeFile(join(directory, MANIFEST), JSON.stringify(manifest));
  await assert.rejects(verifyArtifacts(directory, { version: "9.9.9", publicKey }), /not signed/);

  await writeArtifacts(directory, { version, publicKey, signingKey });
  await writeFile(join(directory, ARTIFACTS["agent-macos"]), "tampered");
  await assert.rejects(verifyArtifacts(directory, { version, publicKey }), /agent-macos SHA-256/);
});
