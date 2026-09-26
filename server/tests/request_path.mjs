// Counts the D1 round trips the compiled Worker makes on the dashboard's hot
// paths, and runs its cron cleanup, inside workerd. A test-only wrapper counts
// the Worker's own D1 calls (a batch is one round trip); Durable Objects keep
// the real binding. It is never deployed.
import assert from 'node:assert/strict';
import { createHash, generateKeyPairSync, sign } from 'node:crypto';
import { readdir, readFile } from 'node:fs/promises';
import { fileURLToPath } from 'node:url';
import { Miniflare, convertV4MiniflareOptions } from '../../dashboard/node_modules/miniflare/dist/src/index.js';

const root = fileURLToPath(new URL('../build/', import.meta.url));
const migrations = new URL('../migrations/', import.meta.url);
const { publicKey, privateKey } = generateKeyPairSync('rsa', { modulusLength: 2048 });
const jwk = { ...publicKey.export({ format: 'jwk' }), kid: 'test-key', alg: 'RS256', use: 'sig' };
const token = (organization, permissions = []) => {
  const encode = value => Buffer.from(JSON.stringify(value)).toString('base64url');
  const data = `${encode({ alg: 'RS256', kid: 'test-key' })}.${encode({
    sub: 'user-test', org_id: organization, client_id: 'client-test', iss: 'https://api.workos.com',
    exp: Math.floor(Date.now() / 1000) + 300, permissions,
  })}`;
  return `${data}.${sign('RSA-SHA256', Buffer.from(data), privateKey).toString('base64url')}`;
};
const hash = value => createHash('sha256').update(value).digest('hex');
const wrapper = `
import Server, { AgentCoordinator, CompanyPresence, RemoteSession } from './index.js';
import { WorkerEntrypoint } from 'cloudflare:workers';
let roundTrips = 0;
class D1PreparedStatement {
  constructor(inner) { this.inner = inner; }
  bind(...values) { return new D1PreparedStatement(this.inner.bind(...values)); }
  first(column) { roundTrips++; return this.inner.first(column); }
  run() { roundTrips++; return this.inner.run(); }
  all() { roundTrips++; return this.inner.all(); }
  raw(options) { roundTrips++; return this.inner.raw(options); }
}
// The Worker checks the binding's constructor name.
class D1Database {
  constructor(inner) { this.inner = inner; }
  prepare(query) { return new D1PreparedStatement(this.inner.prepare(query)); }
  batch(statements) { roundTrips++; return this.inner.batch(statements.map(statement => statement.inner)); }
  exec(query) { roundTrips++; return this.inner.exec(query); }
}
export default class extends WorkerEntrypoint {
  constructor(ctx, env) { super(ctx, env); this.server = new Server(ctx, { ...env, DB: new D1Database(env.DB) }); }
  fetch(request) {
    if (new URL(request.url).pathname === '/__test/round-trips') {
      const count = roundTrips; roundTrips = 0; return Response.json({ count });
    }
    return this.server.fetch(request);
  }
  scheduled(controller) { return this.server.scheduled(controller); }
}
export { AgentCoordinator, CompanyPresence, RemoteSession };
`;
const runtime = new Miniflare(convertV4MiniflareOptions({ unsafeTriggerHandlers: true, workers: [{
  name: 'request-path-test',
  modules: [
    { type: 'ESModule', path: `${root}request-path-test.js`, contents: wrapper },
    { type: 'ESModule', path: `${root}index.js` },
    { type: 'CompiledWasm', path: `${root}index_bg.wasm` },
  ],
  compatibilityDate: '2026-08-22', compatibilityFlags: ['nodejs_compat'],
  durableObjects: {
    AGENT_COORDINATOR: { className: 'AgentCoordinator', useSQLite: true },
    COMPANY_PRESENCE: { className: 'CompanyPresence', useSQLite: true },
    REMOTE_SESSION: { className: 'RemoteSession', useSQLite: true },
  }, d1Databases: ['DB'],
  bindings: { PLATFORM_OWNER_USER_IDS: 'owner-test', WORKOS_CLIENT_ID: 'client-test', WORKOS_ISSUER: 'https://api.workos.com', TENANT_ROOT_DOMAIN: 'meshrmm.com', PUBLIC_API_URL: 'https://api.meshrmm.com', DASHBOARD_ORIGIN: 'https://meshrmm.com' },
  outboundService: 'jwks',
}, {
  name: 'jwks', modules: true,
  script: `export default { fetch(request) {
    if (request.url !== 'https://api.workos.com/sso/jwks/client-test') throw new Error('Unexpected external request');
    return Response.json(${JSON.stringify({ keys: [jwk] })}); } };`,
}] }));

const sockets = [];
try {
  const db = await runtime.getD1Database('DB');
  for (const file of (await readdir(migrations)).filter(name => name.endsWith('.sql')).sort()) {
    const sql = await readFile(new URL(file, migrations), 'utf8');
    await db.exec(sql.replace(/--[^\n]*/g, '').replace(/\s*\n\s*/g, ' '));
  }
  await db.exec(`INSERT INTO companies (id, name, created_at, slug, status, workos_organization_id) VALUES ('company-acme', 'Acme', 0, 'acme', 'active', 'org-acme'), ('company-other', 'Other', 0, 'other', 'active', 'org-other'), ('company-legacy', 'Legacy', 0, NULL, 'active', 'org-legacy'), ('company-held', 'Held', 0, 'held', 'suspended', 'org-held'), ('company-new', 'New', 0, 'new', 'awaiting_admin', 'org-new'); INSERT INTO agents (id, company_id, name, auth_token_hash, created_by_user_id, created_at, updated_at, deletion_requested_at) VALUES ('device-acme', 'company-acme', 'Acme PC', '${'a'.repeat(64)}', 'user-test', 0, 0, NULL), ('device-deleted', 'company-acme', 'Old PC', '${'a'.repeat(64)}', 'user-test', 0, 0, 1), ('device-other', 'company-other', 'Other PC', '${'a'.repeat(64)}', 'user-test', 0, 0, NULL);`);
  const now = Date.now();
  // Expired rows the request path used to delete. Only the cron removes them,
  // and only once they are past its grace period.
  const seed = (label, expiresAt) => db.exec(`INSERT INTO agent_event_subscriptions (token_hash, company_id, user_id, created_at, expires_at) VALUES ('${hash(`subscription-${label}`)}', 'company-acme', 'user-test', 0, ${expiresAt}); INSERT INTO remote_handoffs (token_hash, company_id, device_id, user_id, created_at, expires_at) VALUES ('${hash(`handoff-${label}`)}', 'company-acme', 'device-acme', 'user-test', 0, ${expiresAt}); INSERT INTO agent_install_tokens (id, token_hash, company_id, created_by_user_id, platform, created_at, expires_at) VALUES ('installer-${label}', '${hash(`installer-${label}`)}', 'company-acme', 'user-test', 'windows-x64', 0, ${expiresAt});`);
  await seed('stale', now - 11 * 60_000);
  await seed('recent', now - 60_000);
  const count = async (table, where = '1') => (await db.prepare(`SELECT COUNT(*) AS n FROM ${table} WHERE ${where}`).first()).n;
  const roundTrips = async () => (await (await runtime.dispatchFetch('https://test.internal/__test/round-trips')).json()).count;
  const post = async (host, path, jwt, body) => {
    await roundTrips();
    const response = await runtime.dispatchFetch(`https://${host}${path}`, {
      method: 'POST', headers: { 'Content-Type': 'application/json', Authorization: `Bearer ${jwt}` },
      body: body === undefined ? undefined : JSON.stringify(body),
    });
    // Read the body before the next request so an unread one cannot stall it.
    return { status: response.status, body: await response.json(), roundTrips: await roundTrips() };
  };
  const subscribe = (host, organization) => post(host, '/v1/agents/events/subscriptions', token(organization));
  const upgrade = async (host, subscriptionToken) => {
    await roundTrips();
    const response = await runtime.dispatchFetch(`https://${host}/v1/agents/events?protocol=2&token=${subscriptionToken}`, {
      headers: { Upgrade: 'websocket', Origin: host === 'api.meshrmm.com' ? 'https://meshrmm.com' : `https://${host}` },
    });
    if (response.webSocket) { response.webSocket.accept(); sockets.push(response.webSocket); } else { await response.arrayBuffer(); }
    return { status: response.status, roundTrips: await roundTrips() };
  };

  // The first request of an isolate fetches signing keys and records the
  // monthly active user; later requests reuse both.
  for (const [host, organization] of [['acme.meshrmm.com', 'org-acme'], ['localhost', 'org-legacy']]) {
    assert.equal((await subscribe(host, organization)).status, 200);
  }

  // Subscription create: authorization, then the insert. Dashboard
  // authorization resolves api.meshrmm.com as the company hostname "api", so
  // the legacy control-plane hostname is exercised through localhost.
  for (const [host, organization, websocketUrl] of [
    ['acme.meshrmm.com', 'org-acme', 'wss://acme.meshrmm.com/v1/agents/events'],
    ['localhost', 'org-acme', 'wss://acme.meshrmm.com/v1/agents/events'],
    ['localhost', 'org-legacy', 'wss://api.meshrmm.com/v1/agents/events'],
  ]) {
    const { status, body, roundTrips: trips } = await subscribe(host, organization);
    assert.equal(status, 200, `${host} ${organization}`);
    assert.deepEqual(Object.keys(body).sort(), ['expires_at_unix_ms', 'subscription_token', 'websocket_url']);
    assert.equal(body.websocket_url, websocketUrl);
    assert.equal(trips, 2, `subscription round trips on ${host} for ${organization}`);
  }
  let result = await subscribe('held.meshrmm.com', 'org-held');
  assert.equal(result.status, 403, 'a suspended company cannot subscribe');
  assert.equal(await count('agent_event_subscriptions', "company_id = 'company-held'"), 0);
  result = await subscribe('new.meshrmm.com', 'org-new');
  assert.equal(result.status, 200, 'a company awaiting its administrator can subscribe');
  assert.equal((await db.prepare("SELECT status FROM companies WHERE id = 'company-new'").first()).status, 'active');

  // WebSocket upgrade: one claim that also checks the hostname.
  await db.exec(`INSERT INTO agent_event_subscriptions (token_hash, company_id, user_id, created_at, expires_at) VALUES ('${hash('e'.repeat(64))}', 'company-acme', 'user-test', 0, ${now - 1})`);
  assert.equal((await upgrade('acme.meshrmm.com', 'e'.repeat(64))).status, 401, 'an expired subscription is rejected');
  result = await upgrade('unknown.example.com', 'f'.repeat(64));
  assert.equal(result.status, 403, 'an unknown hostname is rejected');
  assert.equal(result.roundTrips, 0, 'an unknown hostname is rejected before D1');
  const ticket = (await subscribe('acme.meshrmm.com', 'org-acme')).body.subscription_token;
  const unused = `token_hash = '${hash(ticket)}' AND used_at IS NULL`;
  result = await upgrade('other.meshrmm.com', ticket);
  assert.equal(result.status, 401, "another company's hostname is rejected");
  assert.equal(await count('agent_event_subscriptions', unused), 1, 'a rejected subscription is not used up');
  result = await upgrade('acme.meshrmm.com', ticket);
  assert.equal(result.status, 101);
  assert.equal(result.roundTrips, 1, 'upgrade round trips');
  assert.equal(await count('agent_event_subscriptions', unused), 0);
  assert.equal((await upgrade('acme.meshrmm.com', ticket)).status, 401, 'a subscription cannot be replayed');
  // The legacy API hostname has the form of a company hostname, but accepts
  // a subscription for any company.
  for (const organization of ['org-legacy', 'org-acme']) {
    const legacyTicket = (await subscribe('localhost', organization)).body.subscription_token;
    assert.equal((await upgrade('api.meshrmm.com', legacyTicket)).status, 101, `the legacy hostname accepts ${organization}`);
  }

  // Handoff create: authorization, then the insert and its audit event in one batch.
  const handoff = (host, organization, device) => post(host, '/v1/remote/handoffs', token(organization), { device_id: device, start_in_background: true });
  result = await handoff('acme.meshrmm.com', 'org-acme', 'device-acme');
  assert.equal(result.status, 200);
  const created = result.body;
  assert.deepEqual(Object.keys(created).sort(), ['api_url', 'expires_at_unix_ms', 'handoff_token', 'start_in_background']);
  assert.equal(created.api_url, 'https://acme.meshrmm.com');
  assert.equal(created.start_in_background, true);
  assert.equal(result.roundTrips, 2, 'handoff round trips');
  assert.deepEqual(await db.prepare(`SELECT company_id, device_id, start_in_background FROM remote_handoffs WHERE token_hash = '${hash(created.handoff_token)}'`).first(), { company_id: 'company-acme', device_id: 'device-acme', start_in_background: 1 });
  assert.equal(await count('audit_events', "action = 'remote.handoff_create' AND target_id = 'device-acme' AND company_id = 'company-acme'"), 1);
  for (const device of ['device-other', 'device-deleted', 'device-missing']) {
    const handoffs = await count('remote_handoffs');
    const audits = await count('audit_events');
    result = await handoff('acme.meshrmm.com', 'org-acme', device);
    assert.equal(result.status, 404, device);
    assert.equal(result.body.error, 'Agent not found');
    assert.equal(await count('remote_handoffs'), handoffs, `${device} creates no handoff`);
    assert.equal(await count('audit_events'), audits, `${device} records no audit event`);
  }

  // Installer create: authorization, then the insert and its audit event in one batch.
  result = await post('acme.meshrmm.com', '/v1/agent-installers', token('org-acme', ['agents:manage']), { platform: 'windows-x64' });
  assert.equal(result.status, 200);
  const installer = result.body;
  assert.equal(installer.server, 'https://acme.meshrmm.com');
  assert.equal(result.roundTrips, 2, 'installer round trips');
  assert.equal(await count('agent_install_tokens', `token_hash = '${hash(installer.install_token)}'`), 1);
  assert.equal(await count('audit_events', "action = 'agent_installer.issue'"), 1);

  // The requests above purged nothing. The cron purges rows past the grace
  // period from all three tables in one batch and keeps the rest.
  const tables = { agent_event_subscriptions: 'subscription', remote_handoffs: 'handoff', agent_install_tokens: 'installer' };
  const seeded = (table, label) => count(table, `token_hash = '${hash(`${tables[table]}-${label}`)}'`);
  const live = {};
  for (const table of Object.keys(tables)) {
    assert.equal(await seeded(table, 'stale') + await seeded(table, 'recent'), 2, `${table} keeps expired rows until the cron runs`);
    live[table] = await count(table, `expires_at > ${now}`);
  }
  await roundTrips();
  const scheduled = await runtime.dispatchFetch('https://test.internal/cdn-cgi/local/scheduled?cron=*/30+*+*+*+*');
  assert.equal(scheduled.status, 200, await scheduled.text());
  assert.equal(await roundTrips(), 1, 'cleanup round trips');
  for (const table of Object.keys(tables)) {
    assert.equal(await seeded(table, 'stale'), 0, `${table} purges rows past the grace period`);
    assert.equal(await seeded(table, 'recent'), 1, `${table} keeps rows within the grace period`);
    assert.equal(await count(table, `expires_at > ${now}`), live[table], `${table} keeps live rows`);
  }
  console.log('Request path passed: subscription create in 2 D1 round trips with tenant and legacy URLs, suspended and awaiting-admin companies, upgrade in 1 round trip without consuming a wrong-host token, handoff and installer create in 2 with audit rows and 404s, no cleanup on requests, and cron cleanup with its grace period.');
} finally {
  for (const socket of sockets) { try { socket.close(); } catch {} }
  await runtime.dispose();
}
