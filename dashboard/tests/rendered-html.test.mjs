import assert from "node:assert/strict";
import { readFile } from "node:fs/promises";
import test from "node:test";

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

test("starts WorkOS login once and safely restores the home route", async () => {
  const [login, page, providers] = await Promise.all([
    readFile(new URL("../app/login/page.tsx", import.meta.url), "utf8"),
    readFile(new URL("../app/page.tsx", import.meta.url), "utf8"),
    readFile(new URL("../app/providers.tsx", import.meta.url), "utf8"),
  ]);

  assert.match(login, /started\.current/);
  assert.match(login, /LOGIN_ATTEMPT_TTL_MS/);
  assert.match(login, /sessionStorage\.setItem\(LOGIN_ATTEMPT_KEY/);
  assert.match(login, /signIn\(\{ state: \{ returnTo: "\/" \} \}\)/);
  assert.doesNotMatch(page, /window\.location\.pathname === "\/login"/);
  assert.match(providers, /redirectUri=\{redirectUri\}/);
  assert.match(providers, /onRedirectCallback=\{handleRedirectCallback\}/);
  assert.match(providers, /destination\.origin === window\.location\.origin/);
});

test("uses organization-scoped WorkOS widgets and one-time remote handoffs", async () => {
  const [page, providers, layout] = await Promise.all([
    readFile(new URL("../app/page.tsx", import.meta.url), "utf8"),
    readFile(new URL("../app/providers.tsx", import.meta.url), "utf8"),
    readFile(new URL("../app/layout.tsx", import.meta.url), "utf8"),
  ]);

  assert.match(page, /UsersManagement/);
  assert.match(page, /AdminPortalSsoConnection/);
  assert.match(page, /AdminPortalDomainVerification/);
  assert.match(page, /OrganizationSwitcher/);
  assert.match(page, /AuthenticationRedirectStarted/);
  assert.match(page, /tokenError\.message === "No access token available"/);
  assert.match(page, /signIn\(\{ organizationId: organizationId \?\? undefined, state: \{ returnTo: "\/" \} \}\)/);
  assert.match(page, /\/v1\/remote\/handoffs/);
  assert.match(page, /pulsermm:\/\/connect\?handoff=/);
  assert.doesNotMatch(page, /pulsermm:\/\/connect\?device=/);
  assert.doesNotMatch(page, /WORKOS_DEVICE_GRANTS|X-Pulse-User/);
  assert.match(providers, /WorkOsWidgets/);
  assert.doesNotMatch(`${page}\n${providers}\n${layout}`, /chatgpt\.site/i);
});
