import { spawnSync } from "node:child_process";
import { createHash } from "node:crypto";
import { existsSync, readdirSync, readFileSync, writeFileSync } from "node:fs";
import { dirname, join, resolve } from "node:path";
import { createInterface } from "node:readline/promises";
import { fileURLToPath } from "node:url";

import { checkHealth, healthUrl } from "./deploy-server.mjs";
import { compareVersions } from "./release-config.mjs";

const repositoryRoot = resolve(dirname(fileURLToPath(import.meta.url)), "..");
const serverDirectory = resolve(repositoryRoot, "server");
const wrangler = resolve(repositoryRoot, "dashboard/node_modules/.bin/wrangler");
const releaseConfigPath = resolve(repositoryRoot, "release.json");

export const usage = `Usage: node scripts/deploy-prod.mjs [options]

Deploys main to production and verifies it:
  1. Preflight: clean main that matches origin/main, green CI on that commit,
     GitHub and Cloudflare sign-ins, every Worker secret the server reads is set,
     and a server dry run builds.
  2. Deploys the control plane Worker (D1 migrations first, then /healthz).
  3. When anything outside server/docs changed since the last release, bumps
     release.json in a release pull request, squash-merges it into main once its
     required checks pass, and waits for the Publish native release workflow.
  4. Verifies /healthz, meshrmm.com and admin.meshrmm.com, and that every
     download in the live update manifest has the released version and SHA-256.

Options:
  --dry-run          Run the preflight and print the plan. Changes nothing.
  --yes, -y          Don't ask for confirmation before deploying.
  --message <text>   Release summary: "chore(release): publish <text> <version>".
  --version <x.y.z>  Release this version instead of the next patch version.
  --minor, --major   Bump the minor or major version instead of the patch.
  --release          Publish a native release even if only server files changed.
  --no-release       Deploy only the server.
  --skip-server      Don't deploy the server.
  --verify-only      Only verify what is live now.`;

export function parseArguments(argv) {
  const options = {
    dryRun: false,
    yes: false,
    message: undefined,
    version: undefined,
    bump: "patch",
    release: "auto",
    server: true,
    verifyOnly: false,
  };
  for (let index = 0; index < argv.length; index++) {
    const argument = argv[index];
    const value = () => {
      const next = argv[++index];
      if (next === undefined || next.startsWith("--")) throw new Error(`${argument} needs a value\n\n${usage}`);
      return next;
    };
    if (argument === "--dry-run") options.dryRun = true;
    else if (argument === "--yes" || argument === "-y") options.yes = true;
    else if (argument === "--message") options.message = value();
    else if (argument === "--version") options.version = value();
    else if (argument === "--minor" || argument === "--major") options.bump = argument.slice(2);
    else if (argument === "--release") options.release = "always";
    else if (argument === "--no-release") options.release = "never";
    else if (argument === "--skip-server") options.server = false;
    else if (argument === "--verify-only") options.verifyOnly = true;
    else if (argument === "--help" || argument === "-h") return { help: true };
    else throw new Error(`Unknown argument ${JSON.stringify(argument)}\n\n${usage}`);
  }
  if (options.version !== undefined) compareVersions(options.version, options.version);
  if (!options.server && options.release === "never" && !options.verifyOnly) {
    throw new Error("--skip-server with --no-release leaves nothing to deploy; use --verify-only.");
  }
  return options;
}

export function bumpVersion(version, bump) {
  const [major, minor, patch] = version.split(/[-+]/)[0].split(".").map(Number);
  if (bump === "major") return `${major + 1}.0.0`;
  if (bump === "minor") return `${major}.${minor + 1}.0`;
  return `${major}.${minor}.${patch + 1}`;
}

// Picks the version to publish. It must be newer than both release.json and the live
// manifest, so neither the workflow's version check nor installed Agents reject it.
export function nextVersion({ configured, published, requested, bump }) {
  const newest = published && compareVersions(published, configured) > 0 ? published : configured;
  const version = requested ?? bumpVersion(newest, bump);
  if (compareVersions(version, newest) <= 0) {
    throw new Error(`release version ${version} must be greater than ${newest}`);
  }
  return version;
}

// Files that ship only in the server Worker or never ship. Anything else is part of the
// Agent, the viewers or the dashboard, which reach production only through a native release.
export function affectsNativeRelease(path) {
  return !(
    /^(server|docs|dist|scripts\/tests)\//.test(path) ||
    /\.md$/i.test(path) ||
    /\.test\.mjs$/.test(path) ||
    path === "scripts/deploy-server.mjs" ||
    path === "scripts/deploy-prod.mjs" ||
    path === ".github/workflows/ci.yml"
  );
}

export function secretsReadByServer(sources) {
  const names = new Set();
  for (const source of sources) {
    for (const match of source.matchAll(/\.secret\("([A-Z0-9_]+)"\)/g)) names.add(match[1]);
  }
  return [...names].sort();
}

export function releaseCommitMessage(version, message) {
  return message ? `chore(release): publish ${message} ${version}` : `chore(release): publish ${version}`;
}

export const releaseBranch = (version) => `release/${version}`;

const failedConclusions = new Set(["FAILURE", "CANCELLED", "TIMED_OUT", "ACTION_REQUIRED", "STARTUP_FAILURE", "ERROR"]);

// Reads a release pull request's state from `gh pr view --json state,mergeCommit,
// mergeStateStatus,autoMergeRequest,statusCheckRollup`. Returns { merged: sha } once it is
// squash-merged, { problem } when it can no longer merge on its own, or { waiting: summary }.
export function releasePullRequestProgress(pr) {
  if (pr.state === "MERGED") return { merged: pr.mergeCommit.oid };
  if (pr.state === "CLOSED") return { problem: "it was closed without merging" };
  const checks = pr.statusCheckRollup ?? [];
  const failed = checks.filter((check) => failedConclusions.has(check.conclusion || check.state));
  if (failed.length) return { problem: `checks failed: ${failed.map((check) => check.name ?? check.context).join(", ")}` };
  if (pr.mergeStateStatus === "DIRTY") return { problem: "it conflicts with main" };
  if (pr.mergeStateStatus === "BEHIND") return { problem: "main moved on; the required checks must run against the new main" };
  if (!pr.autoMergeRequest) return { problem: "auto-merge was disabled" };
  // Check runs report status and conclusion; commit statuses report only state.
  const done = checks.filter((check) => (check.status ? check.status === "COMPLETED" : check.state !== "PENDING")).length;
  return { waiting: checks.length ? `${done}/${checks.length} checks finished` : "waiting for checks to start" };
}

export function withVersion(configText, version) {
  const updated = configText.replace(/("version"\s*:\s*")[^"]*(")/, `$1${version}$2`);
  if (updated === configText && !configText.includes(`"${version}"`)) {
    throw new Error("release.json has no version field");
  }
  return updated;
}

// Checks every release in the live manifest against the expected version and downloads
// each asset to compare its SHA-256. Returns a list of problems; empty means verified.
export async function verifyManifest(manifest, version, { fetch = globalThis.fetch } = {}) {
  const problems = [];
  const releases = Object.entries(manifest?.releases ?? {});
  if (manifest?.schema_version !== 1 || releases.length === 0) return ["the update manifest has no releases"];
  for (const [target, release] of releases) {
    if (release.version !== version) {
      problems.push(`${target} is ${release.version}, expected ${version}`);
      continue;
    }
    try {
      const response = await fetch(cacheBusted(release.url), { headers: { "Cache-Control": "no-store" } });
      if (!response.ok) {
        problems.push(`${release.url} returned HTTP ${response.status}`);
        continue;
      }
      const digest = createHash("sha256").update(Buffer.from(await response.arrayBuffer())).digest("hex");
      if (digest !== release.sha256) problems.push(`${target} download does not match its manifest SHA-256`);
    } catch (error) {
      problems.push(`${release.url}: ${error.message}`);
    }
  }
  return problems;
}

function cacheBusted(url) {
  const busted = new URL(url);
  busted.searchParams.set("t", Date.now().toString());
  return busted.href;
}

const sleep = (milliseconds) => new Promise((done) => setTimeout(done, milliseconds));

function heading(text) {
  console.log(`\n== ${text}`);
}

function command(program, args, { cwd = repositoryRoot, inherit = false, allowFailure = false } = {}) {
  const result = spawnSync(program, args, {
    cwd,
    encoding: "utf8",
    stdio: inherit ? "inherit" : ["ignore", "pipe", "pipe"],
    maxBuffer: 64 * 1024 * 1024,
  });
  if (result.error) throw result.error;
  if (result.status !== 0 && !allowFailure) {
    const output = inherit ? "" : `\n${(result.stderr || result.stdout || "").trim()}`;
    throw new Error(`${program} ${args.join(" ")} exited with ${result.status}${output}`);
  }
  return { status: result.status, stdout: (result.stdout ?? "").trim() };
}

const git = (...args) => command("git", args).stdout;

function ghJson(args) {
  return JSON.parse(command("gh", args).stdout || "null");
}

async function fetchManifest(origin) {
  const response = await fetch(cacheBusted(`${origin}/downloads/update-manifest.json`), {
    headers: { "Cache-Control": "no-store" },
  });
  if (!response.ok) throw new Error(`the live update manifest returned HTTP ${response.status}`);
  return response.json();
}

function publishedVersion(manifest) {
  const versions = Object.values(manifest?.releases ?? {}).map((release) => release.version);
  return versions.sort(compareVersions).at(-1);
}

// Waits for a workflow run on `sha` to appear, then watches it and fails unless it succeeds.
async function waitForRun(workflow, sha, label) {
  let run;
  for (let attempt = 0; attempt < 24 && !run; attempt++) {
    if (attempt > 0) await sleep(5000);
    [run] = ghJson(["run", "list", "--workflow", workflow, "--commit", sha, "--event", "push", "--limit", "1",
      "--json", "databaseId,status,conclusion,url"]);
  }
  if (!run) throw new Error(`no ${label} run started for ${sha.slice(0, 7)} within two minutes`);
  if (run.status === "completed") {
    if (run.conclusion !== "success") throw new Error(`${label} ${run.conclusion} for ${sha.slice(0, 7)}: ${run.url}`);
    console.log(`${label} passed: ${run.url}`);
    return run;
  }
  console.log(`Waiting for ${label}: ${run.url}`);
  console.log("(If the production environment requires approval, approve the deploy job there.)");
  const watched = command("gh", ["run", "watch", String(run.databaseId), "--exit-status", "--compact", "--interval", "20"], {
    inherit: true,
    allowFailure: true,
  });
  if (watched.status !== 0) {
    throw new Error(`${label} failed: ${run.url}\nAfter fixing a transient failure: gh run rerun ${run.databaseId} --failed`);
  }
  console.log(`${label} passed: ${run.url}`);
  return run;
}

// Opens the release pull request, enables squash auto-merge and waits until main has the
// squash commit. main requires a pull request, squash merges and green checks on an
// up-to-date branch, so the release can't be pushed to main directly.
async function mergeReleasePullRequest(version, message) {
  const branch = releaseBranch(version);
  const title = releaseCommitMessage(version, message);
  git("switch", "--quiet", "-c", branch);
  try {
    writeFileSync(releaseConfigPath, withVersion(readFileSync(releaseConfigPath, "utf8"), version));
    command("node", ["--test", "scripts/release-config.test.mjs"]);
    git("commit", "--quiet", "-m", title, "--", "release.json");
    command("git", ["push", "--quiet", "--set-upstream", "origin", branch], { inherit: true });
  } finally {
    git("restore", "release.json");
    git("switch", "--quiet", "main");
  }
  const head = git("rev-parse", branch);
  const url = command("gh", ["pr", "create", "--base", "main", "--head", branch, "--title", title,
    "--body", `Publishes native release ${version}. Opened by \`scripts/deploy-prod.mjs\`.`]).stdout.split("\n").at(-1);
  const number = url.split("/").at(-1);
  console.log(`Opened ${url}`);
  command("gh", ["pr", "merge", number, "--auto", "--squash", "--match-head-commit", head,
    "--subject", `${title} (#${number})`, "--body", ""]);

  const started = Date.now();
  let last;
  for (;;) {
    const progress = releasePullRequestProgress(ghJson(["pr", "view", number, "--json",
      "state,mergeCommit,mergeStateStatus,autoMergeRequest,statusCheckRollup"]));
    if (progress.merged) {
      console.log(`Squash-merged ${url} as ${progress.merged.slice(0, 7)}`);
      git("fetch", "--quiet", "origin", "main");
      git("merge", "--quiet", "--ff-only", "origin/main");
      git("branch", "--quiet", "-D", branch);
      return progress.merged;
    }
    if (progress.problem || Date.now() - started > 2 * 60 * 60 * 1000) {
      throw new Error(
        `release pull request ${url} did not merge: ${progress.problem ?? "timed out after two hours"}.\n` +
          `Merge it once it can, or close it and delete ${branch} before rerunning.`,
      );
    }
    if (progress.waiting !== last) console.log(`Waiting for the release pull request to merge: ${(last = progress.waiting)}`);
    await sleep(20000);
  }
}

async function confirm(question) {
  if (!process.stdin.isTTY) throw new Error("Not running in a terminal; pass --yes to deploy without confirmation.");
  const prompt = createInterface({ input: process.stdin, output: process.stdout });
  try {
    return /^y(es)?$/i.test((await prompt.question(`${question} [y/N] `)).trim());
  } finally {
    prompt.close();
  }
}

// The Worker builds for wasm32. rustup installs that target from rust-toolchain.toml; a
// rustc that rustup doesn't manage, such as Homebrew's, ignores the file and lacks it.
function checkWasmToolchain() {
  const sysroot = command("rustc", ["--print", "sysroot"], { cwd: serverDirectory }).stdout;
  if (!existsSync(join(sysroot, "lib", "rustlib", "wasm32-unknown-unknown"))) {
    throw new Error(
      `The rustc on PATH (${sysroot}) has no wasm32-unknown-unknown target. Put rustup's ` +
        "cargo and rustc first on PATH, or remove the other Rust installation.",
    );
  }
}

function serverSources(directory = resolve(serverDirectory, "src")) {
  return readdirSync(directory, { recursive: true })
    .filter((file) => file.endsWith(".rs"))
    .map((file) => readFileSync(join(directory, file), "utf8"));
}

async function preflight(options) {
  heading("Preflight");
  const branch = git("rev-parse", "--abbrev-ref", "HEAD");
  if (branch !== "main") throw new Error(`check out main first (on ${branch})`);
  if (git("status", "--porcelain")) throw new Error("the working tree has uncommitted changes; commit or stash them first");
  git("fetch", "--quiet", "origin", "main");
  const head = git("rev-parse", "HEAD");
  const remote = git("rev-parse", "origin/main");
  if (head !== remote) {
    const [behind, ahead] = git("rev-list", "--left-right", "--count", "origin/main...HEAD").split(/\s+/);
    throw new Error(
      `main is ${ahead} ahead and ${behind} behind origin/main. Production deploys only what is on ` +
        "origin/main and has passed CI: pull, or merge your commits through a pull request first.",
    );
  }
  console.log(`main is at ${head.slice(0, 7)}, matching origin/main`);

  command("gh", ["auth", "status"]);
  if (!existsSync(wrangler)) throw new Error("Wrangler is missing; run `npm ci` in dashboard/ first.");
  command(wrangler, ["whoami"], { cwd: serverDirectory });
  console.log("GitHub and Cloudflare sign-ins work");

  await waitForRun("ci.yml", head, "CI");

  const config = JSON.parse(readFileSync(releaseConfigPath, "utf8"));
  const origin = config.download_origin.replace(/\/$/, "");
  const published = publishedVersion(await fetchManifest(origin));
  console.log(`release.json is ${config.version}; production serves ${published ?? "no release"}`);

  let release = options.release === "always";
  let reason = "requested with --release";
  if (options.release === "auto") {
    const lastRelease = git("log", "-1", "--format=%H", "--", "release.json");
    const changed = git("diff", "--name-only", `${lastRelease}..HEAD`).split("\n").filter(Boolean);
    const shipped = changed.filter(affectsNativeRelease);
    if (published !== config.version) {
      release = true;
      reason = `production serves ${published}, not release.json's ${config.version}`;
    } else if (shipped.length > 0) {
      release = true;
      reason = `${shipped.length} Agent/viewer/dashboard file(s) changed since ${lastRelease.slice(0, 7)}, e.g. ${shipped.slice(0, 3).join(", ")}`;
    } else {
      reason = `nothing outside server/docs changed since the last release (${lastRelease.slice(0, 7)})`;
    }
  }
  const version = release
    ? nextVersion({ configured: config.version, published, requested: options.version, bump: options.bump })
    : undefined;
  // A failed earlier attempt at the same version leaves its release branch behind.
  if (release && git("ls-remote", "--heads", "origin", releaseBranch(version))) {
    throw new Error(
      `origin already has ${releaseBranch(version)}. Close its pull request and delete the branch, or pass --version.`,
    );
  }

  if (options.server) {
    checkWasmToolchain();
    const needed = secretsReadByServer(serverSources());
    const set = new Set(
      JSON.parse(command(wrangler, ["secret", "list", "--format", "json"], { cwd: serverDirectory }).stdout).map(
        (secret) => secret.name,
      ),
    );
    const missing = needed.filter((name) => !set.has(name));
    if (missing.length) {
      throw new Error(`Worker secrets are not set: ${missing.join(", ")}. Set them with \`wrangler secret put\` in server/.`);
    }
    console.log(`All ${needed.length} Worker secrets the server reads are set`);

    heading("Server dry run");
    command("node", ["scripts/deploy-server.mjs", "--dry-run"], { inherit: true });
  }

  return { head, origin, release, reason, version };
}

async function verify(expectedVersion) {
  heading("Verify production");
  const problems = [];
  const config = JSON.parse(readFileSync(releaseConfigPath, "utf8"));
  const origin = config.download_origin.replace(/\/$/, "");
  const version = expectedVersion ?? config.version;

  try {
    const url = healthUrl(readFileSync(resolve(serverDirectory, "wrangler.jsonc"), "utf8"));
    const health = await checkHealth(url);
    console.log(`ok   ${url}: schema ${health.schema?.applied}`);
  } catch (error) {
    problems.push(error.message);
  }

  for (const site of [origin, config.download_origin.replace("://", "://admin.").replace(/\/$/, "")]) {
    try {
      const response = await fetch(site, { redirect: "follow" });
      if (response.ok) console.log(`ok   ${site}: HTTP ${response.status}`);
      else problems.push(`${site} returned HTTP ${response.status}`);
    } catch (error) {
      problems.push(`${site}: ${error.message}`);
    }
  }

  // Cloudflare can serve the previous manifest for a short while after a deploy.
  let manifestProblems = [];
  for (let attempt = 1; attempt <= 6; attempt++) {
    try {
      manifestProblems = await verifyManifest(await fetchManifest(origin), version);
    } catch (error) {
      manifestProblems = [error.message];
    }
    if (manifestProblems.length === 0 || attempt === 6) break;
    await sleep(10000);
  }
  if (manifestProblems.length === 0) console.log(`ok   every download in the update manifest is ${version} and matches its SHA-256`);
  problems.push(...manifestProblems);

  if (problems.length) {
    for (const problem of problems) console.error(`FAIL ${problem}`);
    throw new Error(`production verification failed (${problems.length} problem(s))`);
  }
  console.log("\nProduction verified.");
}

async function deploy(options) {
  if (options.verifyOnly) return verify();

  const plan = await preflight(options);
  heading("Plan");
  console.log(`Server Worker: ${options.server ? "deploy (migrations, Worker, /healthz)" : "skip"}`);
  console.log(`Native release: ${plan.release ? `publish ${plan.version}` : "skip"} (${plan.reason})`);
  if (plan.release) {
    console.log(`Release pull request: ${releaseBranch(plan.version)} -> main, squash-merged as ` +
      `"${releaseCommitMessage(plan.version, options.message)} (#<number>)"`);
  }

  if (options.dryRun) {
    console.log("\nDry run: nothing was deployed.");
    return;
  }
  if (!options.yes && !(await confirm("Deploy to production?"))) {
    console.log("Cancelled.");
    return;
  }

  if (options.server) {
    heading("Deploy server");
    command("node", ["scripts/deploy-server.mjs"], { inherit: true });
  }

  if (plan.release) {
    heading(`Publish native release ${plan.version}`);
    const sha = await mergeReleasePullRequest(plan.version, options.message);
    await waitForRun("native-release-build.yml", sha, "Publish native release");
    await waitForRun("ci.yml", sha, "CI");
  }

  await verify(plan.release ? plan.version : undefined);
}

if (process.argv[1] && resolve(process.argv[1]) === fileURLToPath(import.meta.url)) {
  try {
    const options = parseArguments(process.argv.slice(2));
    if (options.help) console.log(usage);
    else await deploy(options);
  } catch (error) {
    console.error(`\n${error.message}`);
    process.exitCode = 1;
  }
}
