// The Agent and viewer builds a server release ships, described by the
// artifacts.json beside them. The server reads that file to serve the builds
// and to write its update manifest; each build's signature lets installed
// Agents and viewers trust an update whichever server offers it.
//
//   node scripts/release-artifacts.mjs write <dir>
//     Describes the builds in <dir>, signed when MESHRMM_RELEASE_SIGNING_KEY
//     holds the release key's PKCS #8 PEM, unsigned otherwise.
//   node scripts/release-artifacts.mjs verify <dir> [--allow-unsigned]
//     Checks that <dir> holds exactly a complete, signed release.
//   node scripts/release-artifacts.mjs generate-key <private-key.pem>
//     Writes a new signing key and prints its public key.
import { createHash, createPrivateKey, createPublicKey, generateKeyPairSync, sign, verify } from "node:crypto";
import { readdir, readFile, writeFile } from "node:fs/promises";
import { join, resolve } from "node:path";
import { fileURLToPath } from "node:url";

import { readReleaseConfig } from "./release-config.mjs";

export const MANIFEST = "artifacts.json";
export const SCHEMA_VERSION = 1;

// Each update target (see crates/self-update) and its file.
export const ARTIFACTS = {
  "agent-windows-x64": "meshrmm-agent-windows-x64.exe",
  "client-windows-x64": "meshrmm-remote-windows-x64.exe",
  "client-macos-arm64": "meshrmm-remote-macos-arm64.zip",
  "agent-macos": "meshrmm-agent-macos.zip",
};

// What a signature covers; crates/self-update's `signed_message` must match.
export function signedMessage(target, version, sha256) {
  return `meshrmm-release-v1\n${target}\n${version}\n${sha256.toLowerCase()}\n`;
}

// The raw Ed25519 public key of `key`, in hexadecimal.
export function publicKeyHex(key) {
  return Buffer.from(createPublicKey(key).export({ format: "jwk" }).x, "base64url").toString("hex");
}

function publicKeyObject(hex) {
  return createPublicKey({ key: { kty: "OKP", crv: "Ed25519", x: Buffer.from(hex, "hex").toString("base64url") }, format: "jwk" });
}

async function sha256(path) {
  return createHash("sha256").update(await readFile(path)).digest("hex");
}

// Writes `directory`/artifacts.json for the builds it contains. With
// `signingKey`, which must match `publicKey`, every build is signed.
export async function writeArtifacts(directory, { version, publicKey, signingKey }) {
  let key = null;
  if (signingKey) {
    key = createPrivateKey(signingKey);
    if (publicKeyHex(key) !== publicKey) {
      throw new Error(
        "MESHRMM_RELEASE_SIGNING_KEY is not the key in release.json (or MESHRMM_RELEASE_PUBLIC_KEY)",
      );
    }
  }
  const present = new Set(await readdir(directory));
  const artifacts = {};
  for (const [target, file] of Object.entries(ARTIFACTS)) {
    if (!present.has(file)) continue;
    const digest = await sha256(join(directory, file));
    artifacts[target] = { file, sha256: digest };
    if (key) {
      artifacts[target].signature = sign(null, Buffer.from(signedMessage(target, version, digest)), key).toString("hex");
    }
  }
  if (Object.keys(artifacts).length === 0) {
    throw new Error(`${directory} contains none of: ${Object.values(ARTIFACTS).join(", ")}`);
  }
  const manifest = { schema_version: SCHEMA_VERSION, version, artifacts };
  await writeFile(join(directory, MANIFEST), `${JSON.stringify(manifest, null, 2)}\n`, "utf8");
  return manifest;
}

// Throws unless `directory` holds every build of `version`, nothing else, and
// an artifacts.json whose checksums match and whose signatures verify with
// `publicKey`. `allowUnsigned` accepts builds without a signature.
export async function verifyArtifacts(directory, { version, publicKey, allowUnsigned = false }) {
  const manifest = JSON.parse(await readFile(join(directory, MANIFEST), "utf8"));
  if (manifest.schema_version !== SCHEMA_VERSION) {
    throw new Error(`${MANIFEST} schema must be ${SCHEMA_VERSION}`);
  }
  if (manifest.version !== version) {
    throw new Error(`${MANIFEST} version ${manifest.version} does not match ${version}`);
  }
  const key = publicKeyObject(publicKey);
  const targets = Object.keys(manifest.artifacts ?? {});
  for (const [target, file] of Object.entries(ARTIFACTS)) {
    const artifact = manifest.artifacts?.[target];
    if (!artifact) throw new Error(`${MANIFEST} is missing ${target}`);
    if (artifact.file !== file) throw new Error(`${target} must be ${file}`);
    if (artifact.sha256 !== (await sha256(join(directory, file)))) {
      throw new Error(`${target} SHA-256 does not match ${file}`);
    }
    if (artifact.signature === undefined && allowUnsigned) continue;
    const message = Buffer.from(signedMessage(target, version, artifact.sha256));
    if (
      typeof artifact.signature !== "string" ||
      !verify(null, message, key, Buffer.from(artifact.signature, "hex"))
    ) {
      throw new Error(`${target} is not signed with the release key`);
    }
  }
  if (targets.length !== Object.keys(ARTIFACTS).length) {
    throw new Error(`${MANIFEST} has unexpected targets`);
  }
  const allowed = new Set([MANIFEST, ...Object.values(ARTIFACTS)]);
  const unexpected = (await readdir(directory)).filter(name => !allowed.has(name));
  if (unexpected.length > 0) {
    throw new Error(`${directory} contains unexpected files: ${unexpected.join(", ")}`);
  }
}

async function main() {
  const [command, argument, option] = process.argv.slice(2);
  if (command === "generate-key" && argument) {
    const { privateKey } = generateKeyPairSync("ed25519");
    await writeFile(resolve(argument), privateKey.export({ type: "pkcs8", format: "pem" }), { mode: 0o600, flag: "wx" });
    console.log(publicKeyHex(privateKey));
    return;
  }
  const config = await readReleaseConfig();
  const directory = argument && resolve(argument);
  if (command === "write" && directory) {
    const signingKey = process.env.MESHRMM_RELEASE_SIGNING_KEY || null;
    const manifest = await writeArtifacts(directory, { ...config, signingKey });
    const targets = Object.keys(manifest.artifacts).join(", ");
    console.log(`Described ${signingKey ? "signed" : "unsigned"} ${targets} ${config.version} in ${join(directory, MANIFEST)}`);
    return;
  }
  if (command === "verify" && directory && (option === undefined || option === "--allow-unsigned")) {
    await verifyArtifacts(directory, { ...config, allowUnsigned: option === "--allow-unsigned" });
    console.log(`Verified every MeshRMM ${config.version} build in ${directory}`);
    return;
  }
  throw new Error(
    "usage: node scripts/release-artifacts.mjs <write <dir> | verify <dir> [--allow-unsigned] | generate-key <private-key.pem>>",
  );
}

if (process.argv[1] && resolve(process.argv[1]) === fileURLToPath(import.meta.url)) {
  await main();
}
