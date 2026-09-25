import assert from "node:assert/strict";
import test from "node:test";
import { handleAuth } from "../worker/auth.ts";
import { authRequest, LoginRequiredError } from "../features/auth/session-client.ts";

const env = { WORKOS_CLIENT_ID: "client_test", DASHBOARD_SESSION_KEY: Buffer.alloc(32, 7).toString("base64") };
const origin = "https://acme.meshrmm.com";
const sessionName = "__Host-meshrmm-session";
const flowName = "__Host-meshrmm-login";
function request(path, body = {}, cookies = "", overrides = {}) {
  return new Request(`${origin}/auth/${path}`, { method: "POST", headers: { origin, "X-MeshRMM-Auth": "1", cookie: cookies, ...overrides }, body: JSON.stringify(body) });
}
function getCookie(response, name) {
  return response.headers.getSetCookie().find((s) => s.startsWith(`${name}=`))?.split(";")[0];
}
function authentication(organization = "org_acme", refresh = "refresh-secret") {
  const claims = { sid: "session_test", org_id: organization, role: "company_admin", roles: ["company_admin"], exp: Math.floor(Date.now() / 1000) + 300 };
  return { access_token: `header.${Buffer.from(JSON.stringify(claims)).toString("base64url")}.signature`, refresh_token: refresh, organization_id: organization, user: { id: "user_test", email: "test@example.com", first_name: "Test" } };
}
async function begin() {
  const response = await handleAuth(request("login", { returnTo: "/settings" }), env, "org_acme");
  const url = new URL((await response.json()).url);
  return { cookies: getCookie(response, flowName), state: url.searchParams.get("state"), url, response };
}
async function login(t) {
  const flow = await begin();
  t.mock.method(globalThis, "fetch", async () => Response.json(authentication()));
  const response = await handleAuth(request("callback", { code: "code_test", state: flow.state }, flow.cookies), env, "org_acme");
  assert.equal(response.status, 200);
  return response;
}

test("PKCE uses the resolved company and host-only secure login cookie", async () => {
  const { url, response } = await begin();
  assert.equal(url.origin, "https://api.workos.com");
  assert.equal(url.searchParams.get("organization_id"), "org_acme");
  assert.equal(url.searchParams.get("redirect_uri"), origin);
  assert.equal(url.searchParams.get("code_challenge_method"), "S256");
  assert.equal(url.searchParams.get("code_challenge").length, 43);
  const cookie = response.headers.get("set-cookie");
  assert.match(cookie, /Path=\/; HttpOnly; Secure; SameSite=Lax; Max-Age=600/);
  assert.doesNotMatch(cookie, /Domain=/);
  assert.equal(response.headers.get("cache-control"), "no-store");
});

test("callback checks state, flow expiry and PKCE before exchanging a code", async (t) => {
  const flow = await begin();
  const fetch = t.mock.method(globalThis, "fetch", async () => { throw new Error("Unexpected fetch"); });
  for (const cookies of ["", flow.cookies]) {
    const result = await handleAuth(request("callback", { code: "code", state: "wrong" }, cookies), env, "org_acme");
    assert.equal(result.status, 400);
  }
  t.mock.method(Date, "now", () => 9e15);
  assert.equal((await handleAuth(request("callback", { code: "code", state: flow.state }, flow.cookies), env, "org_acme")).status, 400);
  assert.equal(fetch.mock.callCount(), 0);
});

test("callback returns only short-lived credentials and reload rotates refresh cookie", async (t) => {
  const flow = await begin();
  let expectedGrant = "authorization_code";
  const fetch = t.mock.method(globalThis, "fetch", async (url, options) => {
    assert.equal(url, "https://api.workos.com/user_management/authenticate");
    const body = JSON.parse(options.body);
    assert.equal(body.grant_type, expectedGrant);
    if (expectedGrant === "authorization_code") {
      const digest = await crypto.subtle.digest("SHA-256", new TextEncoder().encode(body.code_verifier));
      assert.equal(Buffer.from(digest).toString("base64url"), flow.url.searchParams.get("code_challenge"));
    } else assert.equal(body.refresh_token, "refresh-secret");
    return Response.json(authentication("org_acme", expectedGrant === "authorization_code" ? "refresh-secret" : "rotated-secret"));
  });
  const callback = await handleAuth(request("callback", { code: "code", state: flow.state }, flow.cookies), env, "org_acme");
  const data = await callback.json();
  assert.equal(data.returnTo, "/settings");
  assert.equal(data.user.firstName, "Test");
  assert.equal(data.role, "company_admin");
  assert.doesNotMatch(JSON.stringify(data), /refresh-secret|refresh_token|refreshToken/);
  const oldCookie = getCookie(callback, sessionName);
  assert.doesNotMatch(oldCookie, /refresh-secret/);
  expectedGrant = "refresh_token";
  const reload = await handleAuth(request("session", {}, oldCookie), env, "org_acme");
  assert.equal(reload.status, 200);
  assert.notEqual(getCookie(reload, sessionName), oldCookie);
  assert.equal(fetch.mock.callCount(), 2);
});

test("rejects forged cookies, another tenant and cross-origin CSRF", async (t) => {
  const response = await login(t);
  const cookie = getCookie(response, sessionName);
  assert.equal((await handleAuth(request("session", {}, cookie), env, "org_other")).status, 401);
  const other = new Request("https://other.meshrmm.com/auth/session", { method: "POST", headers: { origin: "https://other.meshrmm.com", "X-MeshRMM-Auth": "1", cookie } });
  assert.equal((await handleAuth(other, env, "org_acme")).status, 401);
  assert.equal((await handleAuth(request("session", {}, `${sessionName}=forged`), env, "org_acme")).status, 401);
  for (const path of ["login", "callback", "session", "logout"]) {
    assert.equal((await handleAuth(request(path, {}, cookie, { origin: "https://other.meshrmm.com" }), env, "org_acme")).status, 403);
    assert.equal((await handleAuth(request(path, {}, cookie, { "X-MeshRMM-Auth": "" }), env, "org_acme")).status, 403);
    assert.equal((await handleAuth(new Request(`${origin}/auth/${path}`), env, "org_acme")).status, 405);
  }
});

test("transient failures preserve the cookie; terminal expiry clears it", async (t) => {
  const response = await login(t);
  const cookies = getCookie(response, sessionName);
  for (const status of [408, 429, 500, 502, 503, 504]) {
    t.mock.method(globalThis, "fetch", async () => new Response("unavailable", { status }));
    const result = await handleAuth(request("session", {}, cookies), env, "org_acme");
    assert.equal(result.status, 503);
    assert.equal(result.headers.has("set-cookie"), false);
  }
  t.mock.method(globalThis, "fetch", async () => Response.json({ error: "invalid_grant" }, { status: 400 }));
  const expired = await handleAuth(request("session", {}, cookies), env, "org_acme");
  assert.equal(expired.status, 401);
  assert.match(expired.headers.get("set-cookie"), /Max-Age=0/);
});

test("logout revokes WorkOS session and clears both cookies, including on upstream failure", async (t) => {
  const response = await login(t);
  const cookies = getCookie(response, sessionName);
  const fetch = t.mock.method(globalThis, "fetch", async (url, options) => {
    assert.equal(url.pathname, "/user_management/sessions/logout");
    assert.equal(url.searchParams.get("session_id"), "session_test");
    assert.equal(options.redirect, "manual");
    return new Response(null, { status: 302 });
  });
  const result = await handleAuth(request("logout", {}, cookies), env, "org_acme");
  assert.equal(result.status, 200);
  assert.equal(result.headers.getSetCookie().length, 2);
  assert.ok(result.headers.getSetCookie().every((c) => c.includes("Max-Age=0")));
  assert.equal(fetch.mock.callCount(), 1);
  t.mock.method(globalThis, "fetch", async () => { throw new Error("offline"); });
  const failed = await handleAuth(request("logout", {}, cookies), env, "org_acme");
  assert.equal(failed.status, 503);
  assert.match(failed.headers.get("set-cookie"), /Max-Age=0/);
});

test("login rejects open redirect state and ignores client-supplied organization", async () => {
  const response = await handleAuth(request("login", { returnTo: "https://evil.example", organizationId: "org_other" }), env, "org_acme");
  const url = new URL((await response.json()).url);
  assert.equal(url.searchParams.get("organization_id"), "org_acme");
});

test("client distinguishes expired sessions from temporary failures", async (t) => {
  t.mock.method(globalThis, "fetch", async () => new Response(null, { status: 401 }));
  await assert.rejects(authRequest("session"), LoginRequiredError);
  t.mock.method(globalThis, "fetch", async () => new Response(null, { status: 503 }));
  await assert.rejects(authRequest("session"), (error) => !(error instanceof LoginRequiredError));
});

test("rejects a callback granting a different company", async (t) => {
  const flow = await begin();
  t.mock.method(globalThis, "fetch", async () => Response.json(authentication("org_other")));
  const result = await handleAuth(request("callback", { code: "code", state: flow.state }, flow.cookies), env, "org_acme");
  assert.equal(result.status, 403);
  assert.match(result.headers.get("set-cookie"), /Max-Age=0/);
});

test("client explains rejected sign-ins without exposing upstream error content", async (t) => {
  for (const [status, message] of [[400, /Sign-in expired.*sign in again/], [403, /Accept its invitation from your email/]]) {
    t.mock.method(globalThis, "fetch", async () => Response.json({ error: "sensitive upstream detail" }, { status }));
    await assert.rejects(authRequest("callback"), (error) => message.test(error.message) && !error.message.includes("sensitive"));
  }
});

test("expired encrypted cookies cannot restore a session", async (t) => {
  const response = await login(t);
  const cookies = getCookie(response, sessionName);
  const now = Date.now();
  t.mock.method(Date, "now", () => now + 31 * 24 * 60 * 60 * 1000);
  const result = await handleAuth(request("session", {}, cookies), env, "org_acme");
  assert.equal(result.status, 401);
});

test("platform login can restore a WorkOS session that has an organization", async (t) => {
  const loginResponse = await handleAuth(request("login"), env);
  const url = new URL((await loginResponse.json()).url);
  t.mock.method(globalThis, "fetch", async () => Response.json(authentication()));
  const callback = await handleAuth(request("callback", { code: "code", state: url.searchParams.get("state") }, getCookie(loginResponse, flowName)), env);
  assert.equal(callback.status, 200);
  const session = await handleAuth(request("session", {}, getCookie(callback, sessionName)), env);
  assert.equal(session.status, 200);
});

test("callback redirects stay on this origin", async (t) => {
  const loginResponse = await handleAuth(request("login", { returnTo: "//evil.example/path" }), env, "org_acme");
  const url = new URL((await loginResponse.json()).url);
  t.mock.method(globalThis, "fetch", async () => Response.json(authentication()));
  const response = await handleAuth(request("callback", { code: "code", state: url.searchParams.get("state") }, getCookie(loginResponse, flowName)), env, "org_acme");
  assert.equal((await response.json()).returnTo, "/");
});
