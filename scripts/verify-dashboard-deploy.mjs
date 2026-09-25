import { dirname, resolve } from "node:path";
import { fileURLToPath } from "node:url";

import { verifyReleaseAssets } from "./verify-release-assets.mjs";

const repositoryRoot = resolve(dirname(fileURLToPath(import.meta.url)), "..");
export const defaultManifestPath = resolve(
  repositoryRoot,
  "dashboard/public/downloads/update-manifest.json",
);

// Runs before `npm run deploy` in the dashboard. A deployment replaces every
// published download, and public/downloads is not committed, so only the
// release workflow, which assembles and verifies CI-built artifacts, may deploy.
export async function verifyDashboardDeploy({
  environment = process.env,
  manifestPath = defaultManifestPath,
} = {}) {
  if (environment.MESHRMM_RELEASE_DEPLOY !== "1") {
    throw new Error(
      "The dashboard is deployed only by the Publish native release workflow, because a " +
        "deployment replaces the published Agent and viewer downloads. Run that workflow " +
        "instead, or use `npm run deploy:dry-run` to check a build.",
    );
  }
  try {
    return await verifyReleaseAssets(manifestPath);
  } catch (error) {
    throw new Error(
      `Refusing to deploy without the complete native release in ${dirname(manifestPath)}: ` +
        error.message,
      { cause: error },
    );
  }
}

if (process.argv[1] && resolve(process.argv[1]) === fileURLToPath(import.meta.url)) {
  try {
    const version = await verifyDashboardDeploy();
    console.log(`Deploying the dashboard with the native downloads for MeshRMM ${version}`);
  } catch (error) {
    console.error(error.message);
    process.exitCode = 1;
  }
}
