import { readFile } from "node:fs/promises";
import { dirname, resolve } from "node:path";
import { fileURLToPath } from "node:url";

const repositoryRoot = resolve(dirname(fileURLToPath(import.meta.url)), "..");
const configPath = resolve(repositoryRoot, "release.json");

const SEMVER = /^\d+\.\d+\.\d+(?:-[0-9A-Za-z.-]+)?(?:\+[0-9A-Za-z.-]+)?$/;

export async function readReleaseConfig(environment = process.env) {
  const config = JSON.parse(await readFile(configPath, "utf8"));
  if (typeof config.version !== "string" || !SEMVER.test(config.version)) {
    throw new Error("release.json version must be a semantic version");
  }
  // Development builds embed their own key in place of the release key; see
  // docs/releases.md. The Rust build script honors the same variable.
  const publicKey = environment.MESHRMM_RELEASE_PUBLIC_KEY || config.signing_public_key;
  if (typeof publicKey !== "string" || !/^[0-9a-fA-F]{64}$/.test(publicKey)) {
    throw new Error("the release signing public key must be 32 bytes in hexadecimal");
  }
  return { version: config.version, publicKey: publicKey.toLowerCase() };
}

function parseVersion(value) {
  const match = /^(\d+)\.(\d+)\.(\d+)(?:-([0-9A-Za-z.-]+))?(?:\+[0-9A-Za-z.-]+)?$/.exec(value);
  if (!match) throw new Error(`invalid semantic version: ${value}`);
  return {
    core: match.slice(1, 4).map(Number),
    prerelease: match[4]?.split(".") ?? [],
  };
}

export function compareVersions(leftValue, rightValue) {
  const left = parseVersion(leftValue);
  const right = parseVersion(rightValue);
  for (let index = 0; index < left.core.length; index += 1) {
    if (left.core[index] !== right.core[index]) {
      return Math.sign(left.core[index] - right.core[index]);
    }
  }
  if (left.prerelease.length === 0 || right.prerelease.length === 0) {
    return left.prerelease.length === right.prerelease.length
      ? 0
      : left.prerelease.length === 0
        ? 1
        : -1;
  }
  const length = Math.max(left.prerelease.length, right.prerelease.length);
  for (let index = 0; index < length; index += 1) {
    const leftPart = left.prerelease[index];
    const rightPart = right.prerelease[index];
    if (leftPart === undefined || rightPart === undefined) {
      return leftPart === rightPart ? 0 : leftPart === undefined ? -1 : 1;
    }
    if (leftPart === rightPart) continue;
    const leftNumeric = /^\d+$/.test(leftPart);
    const rightNumeric = /^\d+$/.test(rightPart);
    if (leftNumeric && rightNumeric) return Math.sign(Number(leftPart) - Number(rightPart));
    if (leftNumeric !== rightNumeric) return leftNumeric ? -1 : 1;
    return leftPart < rightPart ? -1 : 1;
  }
  return 0;
}

// Throws unless `version` is at least the latest published release, so a
// republish cannot roll installed Agents and viewers back.
export function assertNotOlder(version, published) {
  if (compareVersions(version, published) < 0) {
    throw new Error(`release version ${version} is older than the published ${published}`);
  }
}

async function main() {
  const [command, argument] = process.argv.slice(2);
  const config = await readReleaseConfig();

  if (command === "version") {
    console.log(config.version);
    return;
  }
  if (command === "public-key") {
    console.log(config.publicKey);
    return;
  }
  if (command === "assert-newer") {
    if (!argument) throw new Error("assert-newer requires the previous release.json path");
    const previous = JSON.parse(await readFile(resolve(process.cwd(), argument), "utf8"));
    if (typeof previous.version !== "string") {
      throw new Error("previous release.json has no version");
    }
    if (compareVersions(config.version, previous.version) <= 0) {
      throw new Error(
        `release version ${config.version} must be greater than ${previous.version}`,
      );
    }
    console.log(`Release version increased from ${previous.version} to ${config.version}`);
    return;
  }

  if (command === "assert-not-older") {
    if (!argument) throw new Error("assert-not-older requires the published version");
    assertNotOlder(config.version, argument.replace(/^v/, ""));
    console.log(`Release version ${config.version} is not older than the published release`);
    return;
  }

  throw new Error(
    "usage: node scripts/release-config.mjs <version|public-key|assert-newer|assert-not-older> [argument]",
  );
}

if (process.argv[1] && resolve(process.argv[1]) === fileURLToPath(import.meta.url)) {
  await main();
}
