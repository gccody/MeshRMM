// The public WorkOS PKCE flow needs no API secret. Refresh tokens stay in an
// encrypted, host-only HttpOnly cookie and never reach browser JavaScript.
const API = "https://api.workos.com/user_management";
const SESSION_COOKIE = "__Host-meshrmm-session";
const FLOW_COOKIE = "__Host-meshrmm-login";
const SESSION_SECONDS = 30 * 24 * 60 * 60;
const FLOW_SECONDS = 10 * 60;
const encoder = new TextEncoder();

type AuthEnv = { WORKOS_CLIENT_ID: string; DASHBOARD_SESSION_KEY?: string };
type Flow = { state: string; verifier: string; returnTo: string; organizationId?: string; expires: number };
type Session = { refreshToken: string; sessionId: string; organizationId?: string; expires: number };
type Authentication = {
  access_token: string;
  refresh_token: string;
  organization_id?: string;
  user: { id: string; email: string; first_name?: string; last_name?: string; profile_picture_url?: string };
};

function encode(bytes: Uint8Array) {
  return btoa(String.fromCharCode(...bytes)).replace(/\+/g, "-").replace(/\//g, "_").replace(/=+$/, "");
}
function decode(value: string) {
  return Uint8Array.from(atob(value.replace(/-/g, "+").replace(/_/g, "/")), (c) => c.charCodeAt(0));
}
function random() { return encode(crypto.getRandomValues(new Uint8Array(32))); }
function cookie(name: string, value: string, seconds: number) {
  return `${name}=${value}; Path=/; HttpOnly; Secure; SameSite=Lax; Max-Age=${seconds}`;
}
function readCookie(request: Request, name: string) {
  return request.headers.get("cookie")?.split(";").map((part) => part.trim()).find((part) => part.startsWith(`${name}=`))?.slice(name.length + 1);
}
async function key(secret: string) {
  const bytes = decode(secret);
  if (bytes.length !== 32) throw new Error("Invalid dashboard session key");
  return crypto.subtle.importKey("raw", bytes, "AES-GCM", false, ["encrypt", "decrypt"]);
}
async function seal(value: Flow | Session, secret: string, purpose: string) {
  const iv = crypto.getRandomValues(new Uint8Array(12));
  const data = await crypto.subtle.encrypt({ name: "AES-GCM", iv, additionalData: encoder.encode(purpose) }, await key(secret), encoder.encode(JSON.stringify(value)));
  const sealed = `${encode(iv)}.${encode(new Uint8Array(data))}`;
  if (sealed.length > 3800) throw new Error("Session cookie is too large");
  return sealed;
}
async function unseal<T extends { expires: number }>(value: string | undefined, secret: string, purpose: string): Promise<T | null> {
  if (!value || value.length > 3800) return null;
  try {
    const [iv, data] = value.split(".");
    const plaintext = await crypto.subtle.decrypt({ name: "AES-GCM", iv: decode(iv), additionalData: encoder.encode(purpose) }, await key(secret), decode(data));
    const result = JSON.parse(new TextDecoder().decode(plaintext)) as T;
    return Number.isFinite(result.expires) && result.expires > Date.now() ? result : null;
  } catch { return null; }
}
function json(data: unknown, status = 200, cookies: string[] = []) {
  const headers = new Headers({ "Content-Type": "application/json", "Cache-Control": "no-store", "Pragma": "no-cache", "X-Content-Type-Options": "nosniff" });
  for (const value of cookies) headers.append("Set-Cookie", value);
  return new Response(JSON.stringify(data), { status, headers });
}
function clearSession() { return cookie(SESSION_COOKIE, "", 0); }
function clearFlow() { return cookie(FLOW_COOKIE, "", 0); }
function returnPath(value: unknown, origin: string) {
  if (typeof value !== "string") return "/";
  try {
    const url = new URL(value, origin);
    return url.origin === origin ? `${url.pathname}${url.search}${url.hash}` : "/";
  } catch { return "/"; }
}

export async function handleAuth(request: Request, env: AuthEnv, organizationId?: string): Promise<Response> {
  const url = new URL(request.url);
  if (!["/auth/login", "/auth/callback", "/auth/session", "/auth/logout"].includes(url.pathname)) return json({ error: "Not found" }, 404);
  if (request.method !== "POST") return json({ error: "Method not allowed" }, 405);
  // SameSite alone is insufficient: another company's subdomain is same-site.
  if (request.headers.get("origin") !== url.origin || request.headers.get("x-meshrmm-auth") !== "1") return json({ error: "Forbidden" }, 403);
  if (!env.DASHBOARD_SESSION_KEY) return json({ error: "Session service is not configured" }, 503);
  const secret = env.DASHBOARD_SESSION_KEY;
  const sessionPurpose = `${url.origin}:session:${env.WORKOS_CLIENT_ID}`;
  const flowPurpose = `${url.origin}:login:${env.WORKOS_CLIENT_ID}`;
  const readSession = () => unseal<Session>(readCookie(request, SESSION_COOKIE), secret, sessionPurpose);
  try {
    const text = await request.text();
    if (text.length > 8192) return json({ error: "Request too large" }, 413);
    let body: Record<string, unknown>;
    try { body = JSON.parse(text || "{}"); } catch { return json({ error: "Invalid JSON" }, 400); }
    if (!body || typeof body !== "object" || Array.isArray(body)) return json({ error: "Invalid JSON" }, 400);

    if (url.pathname === "/auth/login") {
      const flow: Flow = { state: random(), verifier: random(), returnTo: returnPath(body.returnTo, url.origin), organizationId, expires: Date.now() + FLOW_SECONDS * 1000 };
      const authUrl = new URL(`${API}/authorize`);
      authUrl.search = new URLSearchParams({ client_id: env.WORKOS_CLIENT_ID, provider: "authkit", response_type: "code", redirect_uri: url.origin, state: flow.state, code_challenge: encode(new Uint8Array(await crypto.subtle.digest("SHA-256", encoder.encode(flow.verifier)))), code_challenge_method: "S256" }).toString();
      if (organizationId) authUrl.searchParams.set("organization_id", organizationId);
      if (typeof body.invitationToken === "string") authUrl.searchParams.set("invitation_token", body.invitationToken);
      return json({ url: authUrl.href }, 200, [cookie(FLOW_COOKIE, await seal(flow, secret, flowPurpose), FLOW_SECONDS)]);
    }

    if (url.pathname === "/auth/logout") {
      const session = await readSession();
      if (session) {
        const logout = new URL(`${API}/sessions/logout`);
        logout.searchParams.set("session_id", session.sessionId);
        logout.searchParams.set("return_to", "https://meshrmm.com");
        try {
          const response = await fetch(logout, { redirect: "manual", signal: AbortSignal.timeout(10_000) });
          if (!response.ok && response.status !== 302 && response.status !== 303) return json({ error: "Remote sign-out failed. Your local session has been cleared." }, 502, [clearSession(), clearFlow()]);
        } catch { return json({ error: "Remote sign-out unavailable. Your local session has been cleared." }, 503, [clearSession(), clearFlow()]); }
      }
      return json({ ok: true }, 200, [clearSession(), clearFlow()]);
    }

    let grant: Record<string, string>;
    let flow: Flow | null = null;
    if (url.pathname === "/auth/callback") {
      flow = await unseal<Flow>(readCookie(request, FLOW_COOKIE), secret, flowPurpose);
      if (!flow || typeof body.code !== "string" || !body.code || body.state !== flow.state || flow.organizationId !== organizationId) return json({ error: "Sign-in expired or could not be verified. Please try again." }, 400, [clearFlow()]);
      grant = { grant_type: "authorization_code", code: body.code, code_verifier: flow.verifier };
    } else {
      const session = await readSession();
      if (!session || (organizationId && session.organizationId !== organizationId)) return json({ error: "Sign-in required" }, 401, [clearSession()]);
      grant = { grant_type: "refresh_token", refresh_token: session.refreshToken };
    }
    const response = await fetch(`${API}/authenticate`, {
      method: "POST", headers: { "Content-Type": "application/json" },
      body: JSON.stringify({ client_id: env.WORKOS_CLIENT_ID, ...grant }), signal: AbortSignal.timeout(15_000),
    });
    if (!response.ok) {
      const error = await response.json().catch(() => ({})) as { error?: string };
      // Only a terminal OAuth grant error destroys a session. Network/server
      // failures preserve the cookie so that a later attempt can recover.
      if ((response.status === 400 || response.status === 401) && error.error === "invalid_grant") return json({ error: "Sign-in required" }, 401, [clearSession(), clearFlow()]);
      return json({ error: "Sign-in service temporarily unavailable. Please retry." }, 503);
    }
    const data = await response.json() as Authentication;
    // Claims are decoded only from a direct HTTPS WorkOS response, never from
    // caller-supplied tokens. The control-plane still verifies every API JWT.
    const claims = JSON.parse(new TextDecoder().decode(decode(data.access_token.split(".")[1]))) as { sid: string; org_id?: string; exp: number; role?: string; roles?: string[] };
    if (!data.refresh_token || !data.user?.id || !claims.sid || !Number.isFinite(claims.exp) || claims.exp * 1000 <= Date.now()) throw new Error("Invalid authentication response");
    if ((organizationId && claims.org_id !== organizationId) || data.organization_id !== claims.org_id) return json({ error: "This session does not belong to this company." }, 403, [clearSession(), clearFlow()]);
    const session: Session = { refreshToken: data.refresh_token, sessionId: claims.sid, organizationId: claims.org_id, expires: Date.now() + SESSION_SECONDS * 1000 };
    return json({
      accessToken: data.access_token, expiresAt: claims.exp * 1000,
      user: { id: data.user.id, email: data.user.email, firstName: data.user.first_name, lastName: data.user.last_name, profilePictureUrl: data.user.profile_picture_url },
      organizationId: claims.org_id, role: claims.role, roles: claims.roles,
      ...(flow && { returnTo: flow.returnTo }),
    }, 200, [cookie(SESSION_COOKIE, await seal(session, secret, sessionPurpose), SESSION_SECONDS), ...(flow ? [clearFlow()] : [])]);
  } catch {
    // Never log codes, cookies, tokens, or upstream response bodies.
    return json({ error: "Session service temporarily unavailable. Please retry." }, 503);
  }
}
