import { spawnSync } from "node:child_process";
import { existsSync, readFileSync } from "node:fs";
import { dirname, resolve } from "node:path";
import { fileURLToPath } from "node:url";

const repositoryRoot = resolve(dirname(fileURLToPath(import.meta.url)), "..");
const serverDirectory = resolve(repositoryRoot, "server");
// The dashboard pins the Wrangler version the repository is tested with.
const wrangler = resolve(repositoryRoot, "dashboard/node_modules/.bin/wrangler");

export const usage = `Usage: node scripts/deploy-server.mjs [--dry-run]

Applies pending D1 migrations to the production database, deploys the control
plane Worker, and checks that /healthz reports the schema the Worker expects.
Migrations go first: a Worker never runs against a schema older than its queries.

  --dry-run  List the migrations that would be applied and build the Worker
             without deploying it. Changes nothing.`;

export function parseArguments(argv) {
  let dryRun = false;
  for (const argument of argv) {
    if (argument === "--dry-run") dryRun = true;
    else if (argument === "--help" || argument === "-h") return { help: true };
    else throw new Error(`Unknown argument ${JSON.stringify(argument)}\n\n${usage}`);
  }
  return { dryRun };
}

// Wrangler asks before applying remote migrations; the prompt is kept.
export function steps({ dryRun }) {
  if (dryRun) {
    return [
      { label: "Pending D1 migrations", args: ["d1", "migrations", "list", "DB", "--remote"] },
      { label: "Build without deploying", args: ["deploy", "--dry-run"] },
    ];
  }
  return [
    { label: "Apply D1 migrations", args: ["d1", "migrations", "apply", "DB", "--remote"] },
    { label: "Deploy the Worker", args: ["deploy"] },
  ];
}

export function healthUrl(configText) {
  const match = configText.match(/"PUBLIC_API_URL"\s*:\s*"([^"]+)"/);
  if (!match) throw new Error("server/wrangler.jsonc does not set PUBLIC_API_URL");
  return new URL("/healthz", match[1]).href;
}

// Waits for the new Worker to report a current schema. A deployment takes a
// few seconds to reach every location.
export async function checkHealth(url, { fetch = globalThis.fetch, attempts = 6, delayMs = 5000 } = {}) {
  let last;
  for (let attempt = 1; attempt <= attempts; attempt++) {
    try {
      const response = await fetch(url, { headers: { "Cache-Control": "no-store" } });
      const body = await response.json();
      if (response.ok && body.status === "ok") return body;
      last = `HTTP ${response.status}: ${JSON.stringify(body)}`;
    } catch (error) {
      last = error.message;
    }
    if (attempt < attempts) await new Promise((done) => setTimeout(done, delayMs));
  }
  throw new Error(`${url} did not report a healthy schema: ${last}`);
}

function run({ label, args }) {
  console.log(`\n== ${label}: wrangler ${args.join(" ")}`);
  const result = spawnSync(wrangler, args, { cwd: serverDirectory, stdio: "inherit" });
  if (result.error) throw result.error;
  if (result.status !== 0) throw new Error(`${label} failed (wrangler exited with ${result.status})`);
}

if (process.argv[1] && resolve(process.argv[1]) === fileURLToPath(import.meta.url)) {
  try {
    const options = parseArguments(process.argv.slice(2));
    if (options.help) {
      console.log(usage);
    } else {
      if (!existsSync(wrangler)) throw new Error("Wrangler is missing; run `npm ci` in dashboard/ first.");
      for (const step of steps(options)) run(step);
      if (!options.dryRun) {
        const url = healthUrl(readFileSync(resolve(serverDirectory, "wrangler.jsonc"), "utf8"));
        const health = await checkHealth(url);
        console.log(`\n${url}: ${health.status}, schema ${health.schema.applied}`);
      }
    }
  } catch (error) {
    console.error(error.message);
    process.exitCode = 1;
  }
}
