import { mkdir, readFile, writeFile } from "node:fs/promises";
import { dirname, resolve } from "node:path";
import { fileURLToPath } from "node:url";

const repositoryRoot = resolve(dirname(fileURLToPath(import.meta.url)), "..");
const configPath = resolve(repositoryRoot, "release.json");

export async function readReleaseConfig() {
  const config = JSON.parse(await readFile(configPath, "utf8"));
  if (
    typeof config.version !== "string" ||
    !/^\d+\.\d+\.\d+(?:-[0-9A-Za-z.-]+)?(?:\+[0-9A-Za-z.-]+)?$/.test(config.version)
  ) {
    throw new Error("release.json version must be a semantic version");
  }

  for (const field of ["download_origin", "viewer_server"]) {
    if (typeof config[field] !== "string") {
      throw new Error(`release.json ${field} must be a URL`);
    }
    const url = new URL(config[field]);
    if (url.protocol !== "https:" || !url.hostname) {
      throw new Error(`release.json ${field} must use HTTPS`);
    }
  }

  return {
    version: config.version,
    downloadOrigin: config.download_origin.replace(/\/$/, ""),
    viewerServer: config.viewer_server.replace(/\/$/, ""),
  };
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

async function main() {
  const [command, argument] = process.argv.slice(2);
  const config = await readReleaseConfig();

  if (command === "version") {
    console.log(config.version);
    return;
  }
  if (command === "download-origin") {
    console.log(config.downloadOrigin);
    return;
  }
  if (command === "viewer-config") {
    if (!argument) throw new Error("viewer-config requires an output path");
    const outputPath = resolve(process.cwd(), argument);
    await mkdir(dirname(outputPath), { recursive: true });
    await writeFile(
      outputPath,
      `${JSON.stringify(
        {
          server: config.viewerServer,
          update_manifest_url: `${config.downloadOrigin}/downloads/update-manifest.json`,
        },
        null,
        2,
      )}\n`,
      "utf8",
    );
    console.log(`Wrote release viewer configuration to ${outputPath}`);
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

  throw new Error(
    "usage: node scripts/release-config.mjs <version|download-origin|viewer-config|assert-newer> [argument]",
  );
}

if (process.argv[1] && resolve(process.argv[1]) === fileURLToPath(import.meta.url)) {
  await main();
}
