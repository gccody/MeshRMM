import { createHash } from "node:crypto";
import { readdir, readFile } from "node:fs/promises";
import { basename, dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";

import { readReleaseConfig } from "./release-config.mjs";

const expected = {
  "agent-windows-x64": "meshrmm-agent-windows-x64.exe",
  "client-windows-x64": "meshrmm-remote-windows-x64.exe",
  "client-macos-arm64": "meshrmm-remote-macos-arm64.zip",
};

// Checks that the directory holding `manifestPath` contains exactly the native artifacts for
// release.json's version, with matching checksums and download URLs.
export async function verifyReleaseAssets(manifestPath) {
  const directory = dirname(manifestPath);
  const manifest = JSON.parse(await readFile(manifestPath, "utf8"));
  const config = await readReleaseConfig();

  if (manifest.schema_version !== 1) throw new Error("release manifest schema must be 1");
  for (const [target, filename] of Object.entries(expected)) {
    const release = manifest.releases?.[target];
    if (!release) throw new Error(`release manifest is missing ${target}`);
    if (release.version !== config.version) {
      throw new Error(`${target} version ${release.version} does not match ${config.version}`);
    }
    if (release.url !== `${config.downloadOrigin}/downloads/${filename}`) {
      throw new Error(`${target} URL does not point to ${config.downloadOrigin}/downloads/${filename}`);
    }
    const contents = await readFile(join(directory, filename));
    const checksum = createHash("sha256").update(contents).digest("hex");
    if (release.sha256 !== checksum) throw new Error(`${target} SHA-256 does not match its artifact`);

    if (filename.endsWith(".exe")) {
      const sidecar = (await readFile(join(directory, `${filename}.sha256`), "utf8")).trim();
      if (sidecar !== checksum) throw new Error(`${filename}.sha256 does not match its artifact`);
    }
  }

  if (Object.keys(manifest.releases).length !== Object.keys(expected).length) {
    throw new Error("release manifest contains unexpected targets");
  }
  const allowed = new Set([basename(manifestPath)]);
  for (const filename of Object.values(expected)) {
    allowed.add(filename);
    if (filename.endsWith(".exe")) allowed.add(`${filename}.sha256`);
  }
  const unexpected = (await readdir(directory)).filter(name => !allowed.has(name));
  if (unexpected.length > 0) {
    throw new Error(`release directory contains unexpected files: ${unexpected.join(", ")}`);
  }
  return config.version;
}

if (process.argv[1] && resolve(process.argv[1]) === fileURLToPath(import.meta.url)) {
  const manifestPath = resolve(process.argv[2] ?? "dashboard/public/downloads/update-manifest.json");
  const version = await verifyReleaseAssets(manifestPath);
  console.log(`Verified all native artifacts for MeshRMM ${version}`);
}
