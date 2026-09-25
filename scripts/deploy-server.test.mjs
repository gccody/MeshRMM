import assert from "node:assert/strict";
import { readFile } from "node:fs/promises";
import test from "node:test";

import { checkHealth, healthUrl, parseArguments, steps } from "./deploy-server.mjs";

test("migrations are applied before the Worker is deployed", () => {
  const commands = steps(parseArguments([])).map((step) => step.args.join(" "));
  assert.deepEqual(commands, ["d1 migrations apply DB --remote", "deploy"]);
});

test("a dry run only lists migrations and builds", () => {
  const commands = steps(parseArguments(["--dry-run"])).map((step) => step.args.join(" "));
  assert.deepEqual(commands, ["d1 migrations list DB --remote", "deploy --dry-run"]);
  assert.ok(commands.every((command) => !command.includes(" apply ") && command !== "deploy"));
});

test("unknown arguments are refused", () => {
  assert.throws(() => parseArguments(["--remote"]), /Unknown argument/);
  assert.deepEqual(parseArguments(["--help"]), { help: true });
});

test("the health check uses the configured API origin", async () => {
  const config = await readFile(new URL("../server/wrangler.jsonc", import.meta.url), "utf8");
  assert.equal(healthUrl(config), "https://api.meshrmm.com/healthz");
});

test("the health check waits for a current schema and reports a stale one", async () => {
  const replies = [
    new Response("upstream", { status: 502 }),
    Response.json({ status: "schema_behind", schema: { applied: "0011_x.sql" } }, { status: 503 }),
    Response.json({ status: "ok", schema: { applied: "0012_x.sql" } }),
  ];
  const fetch = async () => replies.shift();
  const health = await checkHealth("https://api.test/healthz", { fetch, delayMs: 0 });
  assert.equal(health.schema.applied, "0012_x.sql");

  const stale = async () => Response.json({ status: "schema_behind" }, { status: 503 });
  await assert.rejects(checkHealth("https://api.test/healthz", { fetch: stale, attempts: 2, delayMs: 0 }), /schema_behind/);
});
