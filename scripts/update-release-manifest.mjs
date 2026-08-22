import { createHash } from "node:crypto";
import { mkdir, readFile, writeFile } from "node:fs/promises";
import { dirname } from "node:path";

const [manifestPath, target, version, url, artifactPath] = process.argv.slice(2);
if (!manifestPath || !target || !version || !url || !artifactPath) {
  throw new Error(
    "usage: node update-release-manifest.mjs <manifest> <target> <version> <url> <artifact>",
  );
}
if (!/^\d+\.\d+\.\d+(?:-[0-9A-Za-z.-]+)?(?:\+[0-9A-Za-z.-]+)?$/.test(version)) {
  throw new Error(`invalid semantic version: ${version}`);
}
if (new URL(url).protocol !== "https:") {
  throw new Error("release URL must use HTTPS");
}

let manifest = { schema_version: 1, releases: {} };
try {
  manifest = JSON.parse(await readFile(manifestPath, "utf8"));
} catch (error) {
  if (error.code !== "ENOENT") throw error;
}
if (manifest.schema_version !== 1 || typeof manifest.releases !== "object") {
  throw new Error("existing update manifest uses an unsupported format");
}

const artifact = await readFile(artifactPath);
manifest.releases[target] = {
  version,
  url,
  sha256: createHash("sha256").update(artifact).digest("hex"),
};
await mkdir(dirname(manifestPath), { recursive: true });
await writeFile(manifestPath, `${JSON.stringify(manifest, null, 2)}\n`, "utf8");
console.log(`Published ${target} ${version} in ${manifestPath}`);
