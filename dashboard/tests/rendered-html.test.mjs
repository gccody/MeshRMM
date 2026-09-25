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

async function render(pathname = "/", hostname = "meshrmm.com", company = null, init = {}, apiFetch = async () => new Response("API")) {
  const workerUrl = new URL("../dist/server/index.js", import.meta.url);
  workerUrl.searchParams.set("test", `${process.pid}-${Date.now()}`);
  const { default: worker } = await import(workerUrl.href);

  return worker.fetch(
    new Request(`https://${hostname}${pathname}`, {
      ...init,
      headers: { accept: "text/html", host: hostname, ...init.headers },
    }),
    {
      ASSETS: { fetch: async () => new Response("Not found", { status: 404 }) },
      WORKOS_CLIENT_ID: "client_test",
      DASHBOARD_SESSION_KEY: Buffer.alloc(32, 7).toString("base64"),
      DB: {
        prepare() {
          return { bind() { return { first: async () => company }; } };
        },
      },
      MESHRMM_API: { fetch: apiFetch },
    },
    { waitUntil() {}, passThroughOnException() {} },
  );
}

test("invitation broker returns the company redirect to the browser without following it in the API", async () => {
  const destination = "https://acme.meshrmm.com/login?invitation_token=test-invitation";
  const response = await render("/login?invitation_token=test-invitation", "auth.meshrmm.com", null, {}, async (request) => {
    assert.equal(request.url, "https://auth.meshrmm.com/v1/auth/invitations/resolve?invitation_token=test-invitation");
    // Model service-binding fetch: automatic redirect following would route
    // the company /login request to the API instead of the dashboard.
    return request.redirect === "manual"
      ? new Response(null, { status: 302, headers: { Location: destination, "Cache-Control": "no-store" } })
      : Response.json({ error: "route not found" }, { status: 404 });
  });
  assert.equal(response.status, 302);
  assert.equal(response.headers.get("location"), destination);
  assert.equal(response.headers.get("cache-control"), "no-store");
});

test("tenant routing preserves the authentication POST body", async () => {
  const response = await render("/auth/login", "acme.meshrmm.com", {
    workos_organization_id: "org_acme",
  }, {
    method: "POST",
    headers: { origin: "https://acme.meshrmm.com", "X-MeshRMM-Auth": "1" },
    body: JSON.stringify({ returnTo: "/settings" }),
  });
  assert.equal(response.status, 200);
  const url = new URL((await response.json()).url);
  assert.equal(url.searchParams.get("organization_id"), "org_acme");
  assert.match(response.headers.get("set-cookie"), /__Host-meshrmm-login=/);
});

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
  assert.match(html, /Company workspace/);
  assert.match(html, /<title>Devices \| MeshRMM<\/title>/);
  assert.doesNotMatch(html, /Fixed company workspace|one-time remote handoffs|Cloudflare live inventory/);
});

test("available sidebar entries are links to their routes", async () => {
  const response = await render("/settings", "acme.meshrmm.com", {
    workos_organization_id: "org_acme",
  });
  const html = await response.text();
  const nav = html.match(/<nav aria-label="Primary navigation">.*?<\/nav>/s)?.[0] ?? "";
  // Devices can open in a new tab; Settings needs a company session first.
  assert.match(nav, /<a href="\/" class="nav-item\s*"><svg[^>]*lucide-monitor/);
  assert.match(nav, /<button type="button" class="nav-item active" aria-current="page" disabled=""><svg[^>]*lucide-settings/);
});

test("settings has a dedicated route and retains tenant isolation", async () => {
  const response = await render("/settings", "acme.meshrmm.com", {
    workos_organization_id: "org_acme",
  });
  assert.equal(response.status, 200);
  const html = await response.text();
  assert.match(html, /<title>Settings \| MeshRMM<\/title>/);
  assert.match(html, /aria-current="page"/);
  assert.match(html, /role="tablist" aria-label="Settings categories"/);
  assert.match(html, /id="settings-tab-dashboard-security" aria-controls="dashboard-security" aria-selected="true"/);
  assert.match(html, /id="settings-tab-remote-sessions" aria-controls="remote-sessions" aria-selected="false"/);
  assert.doesNotMatch(html, /id="idle-timeout"/); // No policy values before account authorization.
  const unknown = await render("/settings", "unknown.meshrmm.com");
  assert.equal(unknown.status, 404);
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
  const updating = { id: "a", name: "Alpha", connected: false, updating_to: "0.3.1" };
  assert.deepEqual(parseAgentEvent({ type: "agent_upsert", revision: 5, agent: updating }), {
    type: "agent_upsert",
    revision: 5,
    agent: updating,
  });
  assert.equal(
    parseAgentEvent({ type: "agent_upsert", revision: 5, agent: { ...updating, updating_to: 3 } }),
    null,
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
