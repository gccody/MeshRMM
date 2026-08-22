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

async function render(pathname = "/") {
  const workerUrl = new URL("../dist/server/index.js", import.meta.url);
  workerUrl.searchParams.set("test", `${process.pid}-${Date.now()}`);
  const { default: worker } = await import(workerUrl.href);

  return worker.fetch(
    new Request(`https://pulsermm.gccody.dev${pathname}`, {
      headers: { accept: "text/html", host: "pulsermm.gccody.dev" },
    }),
    { ASSETS: { fetch: async () => new Response("Not found", { status: 404 }) } },
    { waitUntil() {}, passThroughOnException() {} },
  );
}

test("server-renders the production WorkOS shell without inventory data", async () => {
  const response = await render();
  assert.equal(response.status, 200);
  assert.match(response.headers.get("content-type") ?? "", /^text\/html\b/i);

  const html = await response.text();
  assert.match(html, /<title>Agents \| PulseRMM<\/title>/i);
  assert.match(html, /data-woswidgets-root="true"/);
  assert.match(html, /WorkOS authentication/);
  assert.match(html, /Select company/);
  assert.doesNotMatch(html, /desktop-01|office-pc|sample agent|fake data/i);
});

test("serves the WorkOS initiate-login route", async () => {
  const response = await render("/login");
  assert.equal(response.status, 200);
  assert.match(response.headers.get("content-type") ?? "", /^text\/html\b/i);
  const html = await response.text();
  assert.match(html, /<title>Agents \| PulseRMM<\/title>/i);
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
