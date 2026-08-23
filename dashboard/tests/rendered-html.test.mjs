import assert from "node:assert/strict";
import test from "node:test";
import {
  applyAgentDelta,
  parseAgentEvent,
  parseAgentList,
  sortAgents,
} from "../features/agents/model.ts";
import {
  DEFAULT_IDLE_TIMEOUT_MINUTES,
  formatIdleTimeout,
  hasIdleTimeoutElapsed,
  timeoutMilliseconds,
} from "../features/session/idle-session.ts";

async function render(pathname = "/", hostname = "meshrmm.com", company = null) {
  const workerUrl = new URL("../dist/server/index.js", import.meta.url);
  workerUrl.searchParams.set("test", `${process.pid}-${Date.now()}`);
  const { default: worker } = await import(workerUrl.href);

  return worker.fetch(
    new Request(`https://${hostname}${pathname}`, {
      headers: { accept: "text/html", host: hostname },
    }),
    {
      ASSETS: { fetch: async () => new Response("Not found", { status: 404 }) },
      DB: {
        prepare() {
          return { bind() { return { first: async () => company }; } };
        },
      },
      MESHRMM_API: { fetch: async () => new Response("API") },
    },
    { waitUntil() {}, passThroughOnException() {} },
  );
}

test("server-renders the public marketing site at the root domain", async () => {
  const response = await render();
  assert.equal(response.status, 200);
  assert.match(response.headers.get("content-type") ?? "", /^text\/html\b/i);

  const html = await response.text();
  assert.match(html, /<title>MeshRMM \| Secure remote monitoring<\/title>/i);
  assert.match(html, /data-woswidgets-root="true"/);
  assert.match(html, /Every company gets a private MeshRMM workspace/);
  assert.match(html, /Request an invitation/);
  assert.doesNotMatch(html, /desktop-01|office-pc|sample agent|fake data/i);
});

test("server-renders the owner console only on the admin hostname", async () => {
  const response = await render("/", "admin.meshrmm.com");
  assert.equal(response.status, 200);
  const html = await response.text();
  assert.match(html, /<title>Platform Admin \| MeshRMM<\/title>/i);
  assert.match(html, /Checking administrator access/);
  assert.doesNotMatch(html, /Every company gets a private MeshRMM workspace/);
});

test("resolves a provisioned company before rendering its fixed workspace", async () => {
  const response = await render("/", "acme.meshrmm.com", {
    workos_organization_id: "org_acme",
  });
  assert.equal(response.status, 200);
  const html = await response.text();
  assert.match(html, /Fixed company workspace/);
  assert.match(html, /Company workspace/);
});

test("rejects unknown tenant hostnames before rendering", async () => {
  const response = await render("/", "unknown.meshrmm.com");
  assert.equal(response.status, 404);
  assert.equal(await response.text(), "Company not found");
});

test("serves the WorkOS initiate-login route", async () => {
  const response = await render("/login");
  assert.equal(response.status, 200);
  assert.match(response.headers.get("content-type") ?? "", /^text\/html\b/i);
  const html = await response.text();
  assert.match(html, /<title>MeshRMM \| Secure remote monitoring<\/title>/i);
  assert.match(html, /Preparing secure sign-in/);
  assert.match(html, /Redirecting to WorkOS/);
});

test("sorts and applies live Agent events deterministically", () => {
  const agents = sortAgents([
    { id: "b", name: "Zulu", connected: false },
    { id: "a", name: "Alpha", connected: true },
  ]);
  assert.deepEqual(agents.map((agent) => agent.id), ["a", "b"]);

  const updated = applyAgentDelta(agents, {
    type: "agent_upsert",
    revision: 2,
    agent: { id: "b", name: "Zulu", connected: true },
  });
  assert.deepEqual(updated.map((agent) => [agent.id, agent.connected]), [
    ["a", true],
    ["b", true],
  ]);

  assert.deepEqual(
    applyAgentDelta(updated, { type: "agent_deleted", revision: 3, agent_id: "a" }),
    [{ id: "b", name: "Zulu", connected: true }],
  );
});

test("rejects malformed Agent API and event payloads", () => {
  assert.equal(parseAgentList({ agents: [{ id: "a" }], revision: 1 }), null);
  assert.equal(parseAgentEvent({ type: "agent_deleted", revision: -1, agent_id: "a" }), null);
  assert.deepEqual(
    parseAgentList({
      agents: [{ id: "a", name: "Alpha", connected: true }],
      revision: 4,
    }),
    { agents: [{ id: "a", name: "Alpha", connected: true }], revision: 4 },
  );
});

test("uses a four-hour dashboard idle timeout by default", () => {
  assert.equal(DEFAULT_IDLE_TIMEOUT_MINUTES, 240);
  assert.equal(timeoutMilliseconds(DEFAULT_IDLE_TIMEOUT_MINUTES), 14_400_000);
  assert.equal(formatIdleTimeout(DEFAULT_IDLE_TIMEOUT_MINUTES), "4 hours");
  assert.equal(hasIdleTimeoutElapsed(1_000, 240, 14_400_999), false);
  assert.equal(hasIdleTimeoutElapsed(1_000, 240, 14_401_000), true);
});

test("falls back to the safe idle default for an invalid policy", () => {
  assert.equal(timeoutMilliseconds(0), 14_400_000);
  assert.equal(timeoutMilliseconds(1_441), 14_400_000);
});
