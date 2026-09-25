// Exercises the actual compiled Rust Durable Objects inside workerd. Test-only
// wrappers expose storage/alarms and inject transport failure; never deployed.
import assert from 'node:assert/strict';
import { createHash, generateKeyPairSync, sign } from 'node:crypto';
import { readFile } from 'node:fs/promises';
import { fileURLToPath } from 'node:url';
import { Miniflare, convertV4MiniflareOptions } from '../../dashboard/node_modules/miniflare/dist/src/index.js';

const root = fileURLToPath(new URL('../build/', import.meta.url));
const { publicKey, privateKey } = generateKeyPairSync('rsa', { modulusLength: 2048 });
const jwk = { ...publicKey.export({ format: 'jwk' }), kid: 'test-key', alg: 'RS256', use: 'sig' };
const token = (user = 'user-test', organization = 'org-test', kid = 'test-key') => {
  const encode = value => Buffer.from(JSON.stringify(value)).toString('base64url');
  const data = `${encode({ alg: 'RS256', kid })}.${encode({ sub: user, org_id: organization, client_id: 'client-test', iss: 'https://api.workos.com', exp: Math.floor(Date.now() / 1000) + 300 })}`;
  return `${data}.${sign('RSA-SHA256', Buffer.from(data), privateKey).toString('base64url')}`;
};
const wrapper = `
import Server, { AgentCoordinator as Agent, CompanyPresence as Presence, RemoteSession as Session } from './index.js';
function instrument(Inner) {
  return class {
    constructor(ctx, env) { this.ctx = ctx; this.inner = new Inner(ctx, env); this.statusCalls = 0; }
    async fetch(request) {
      const path = new URL(request.url).pathname;
      if (path === '/__test/state') return Response.json({
        alarm: await this.ctx.storage.getAlarm(),
        presence: await this.ctx.storage.get('presence'),
        delivery: await this.ctx.storage.get('presence_delivery'),
        pendingUpdate: await this.ctx.storage.get('pending_update'),
        session: await this.ctx.storage.get('session'), statusCalls: this.statusCalls,
      });
      if (path === '/__test/fail') { await this.ctx.storage.put('test_failure', await request.json()); return new Response('ok'); }
      if (path === '/__test/alarm') { await this.ctx.storage.deleteAlarm(); return this.inner.alarm(); }
      if (path === '/__test/expire-subscriptions') {
        for (const socket of this.ctx.getWebSockets()) {
          const attachment = socket.deserializeAttachment();
          socket.serializeAttachment(typeof attachment === 'number' ? 1 : { ...attachment, expires_at_unix_ms: 1 });
        }
        return new Response('ok');
      }
      if (path === '/__test/expire-update') {
        const update = await this.ctx.storage.get('pending_update');
        await this.ctx.storage.put('pending_update', { ...update, expires_at_unix_ms: 1 });
        return new Response('ok');
      }
      if (path === '/__test/advance-session') {
        const record = await this.ctx.storage.get('session');
        await this.ctx.storage.put('session', { ...record, expires_at_unix_ms: Date.now() + 900000 });
        return new Response('ok');
      }
      if ((path === '/presence' || path === '/revoke') && await this.ctx.storage.get('test_failure')) return new Response('injected failure', { status: 503 });
      if (path === '/status') this.statusCalls++;
      return this.inner.fetch(request);
    }
    alarm() { return this.inner.alarm(); }
    webSocketMessage(...args) { return this.inner.webSocketMessage(...args); }
    webSocketClose(...args) { return this.inner.webSocketClose(...args); }
    webSocketError(...args) { return this.inner.webSocketError(...args); }
  };
}
export const AgentCoordinator = instrument(Agent);
export const CompanyPresence = instrument(Presence);
export const RemoteSession = instrument(Session);
export default Server;
`;
const runtime = new Miniflare(convertV4MiniflareOptions({ workers: [{
  name: 'presence-test',
  modules: [
    { type: 'ESModule', path: `${root}presence-test.js`, contents: wrapper },
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
  script: `let calls = 0; export default { fetch(request) {
    if (request.url === 'https://api.workos.com/__test/calls') return Response.json({ calls });
    if (request.url !== 'https://api.workos.com/sso/jwks/client-test') throw new Error('Unexpected external request');
    calls++; return Response.json(${JSON.stringify({ keys: [jwk] })}); } };`,
}] }));
const company = 'company-test';
const headers = { 'X-Mesh-Company-Id': company };
const post = (stub, path, body) => stub.fetch(`https://test.internal${path}`, {
  method: 'POST', headers: { ...headers, 'Content-Type': 'application/json' }, body: JSON.stringify(body),
});
const state = async stub => (await stub.fetch('https://test.internal/__test/state')).json();
const pause = () => new Promise(resolve => setTimeout(resolve, 10));
async function until(predicate, message) {
  for (let i = 0; i < 200; i++) { if (await predicate()) return; await pause(); }
  assert.fail(message);
}
const sockets = [];
function collect(response) {
  assert.equal(response.status, 101);
  const socket = response.webSocket;
  const messages = [];
  socket.addEventListener('message', event => messages.push(JSON.parse(event.data)));
  socket.accept(); sockets.push(socket);
  return { socket, messages };
}
try {
  const db = await runtime.getD1Database('DB');
  await db.exec("CREATE TABLE companies (id TEXT PRIMARY KEY, status TEXT, slug TEXT, workos_organization_id TEXT); CREATE TABLE agents (id TEXT PRIMARY KEY, company_id TEXT, name TEXT, deletion_requested_at INTEGER); INSERT INTO companies VALUES ('company-test','active','acme','org-test'); INSERT INTO companies VALUES ('company-other','active','other','org-other'); INSERT INTO agents VALUES ('device-test','company-test','Test PC',NULL);");
  const migration = await readFile(new URL('../migrations/0011_presence_catalog_outbox.sql', import.meta.url), 'utf8');
  await db.exec(migration.replace(/--[^\n]*/g, '').replace(/\n/g, ' '));
  const agents = await runtime.getDurableObjectNamespace('AGENT_COORDINATOR');
  const companies = await runtime.getDurableObjectNamespace('COMPANY_PRESENCE');
  const sessions = await runtime.getDurableObjectNamespace('REMOTE_SESSION');
  const agent = agents.get(agents.idFromName('device-test'));
  const presence = companies.get(companies.idFromName(company));
  const snapshot = async () => (await presence.fetch('https://presence.internal/snapshot', { headers })).json();
  const connectAgent = () => agent.fetch('https://agent.internal/connect', { headers: {
    ...headers, Upgrade: 'websocket', 'X-Mesh-Device-Id': 'device-test', 'X-Mesh-Uninstall-Requested': 'false',
  }});
  const dashboard = collect(await presence.fetch('https://presence.internal/subscribe', { headers: {
    ...headers, Upgrade: 'websocket', 'X-Mesh-Presence-Protocol': '2', 'X-Mesh-User-Id': 'user-test',
  }}));
  await until(() => dashboard.messages.length === 2, 'authorization and initial snapshot');
  assert.equal(dashboard.messages[0].type, 'authorization');
  assert.equal(dashboard.messages[1].agents[0].connected, false);
  const connectionId = dashboard.messages[0].connection_id;
  // Exercise the public renewal route with real RSA verification and a local
  // test JWKS service. No WorkOS credentials or external network are involved.
  const apiRenew = (jwt, host = 'acme.meshrmm.com') => runtime.dispatchFetch(`https://${host}/v1/agents/events/subscriptions/renew`, {
    method: 'POST', headers: { 'Content-Type': 'application/json', ...(jwt ? { Authorization: `Bearer ${jwt}` } : {}) },
    body: JSON.stringify({ connection_id: connectionId, user_id: 'user-test' }),
  });
  assert.equal((await apiRenew()).status, 401);
  assert.equal((await apiRenew(token('another-user'))).status, 410, 'body cannot spoof original user');
  assert.equal((await apiRenew(token(), 'other.meshrmm.com')).status, 403, 'JWT cannot renew another tenant');
  assert.equal((await apiRenew(token())).status, 200);
  // WorkOS keys are cached per isolate, and unknown key IDs cannot force refetches.
  const jwksCalls = async () => (await (await (await runtime.getWorker('jwks')).fetch('https://api.workos.com/__test/calls')).json()).calls;
  assert.equal(await jwksCalls(), 1, 'signing keys are fetched once');
  assert.equal((await apiRenew(token('user-test', 'org-test', 'rotated-key'))).status, 401);
  assert.equal((await apiRenew(token('user-test', 'org-test', 'rotated-key'))).status, 401);
  assert.equal(await jwksCalls(), 1, 'unknown key IDs are rate limited');
  // A database outage is retryable and must not sign the dashboard out.
  await db.exec('ALTER TABLE companies RENAME TO companies_offline');
  const outage = await apiRenew(token());
  await db.exec('ALTER TABLE companies_offline RENAME TO companies');
  assert.equal(outage.status, 503);
  assert.equal(outage.headers.get('Retry-After'), '5');
  assert.doesNotMatch((await outage.json()).error, /sign in/);
  assert.equal((await apiRenew(token())).status, 200);
  // The initial upgrade still consumes a one-use token and overwrites identity headers.
  await db.exec("CREATE TABLE agent_event_subscriptions (token_hash TEXT PRIMARY KEY, company_id TEXT, user_id TEXT, expires_at INTEGER, used_at INTEGER);");
  const ticket = 'b'.repeat(64);
  await db.prepare('INSERT INTO agent_event_subscriptions VALUES (?1, ?2, ?3, ?4, NULL)')
    .bind(createHash('sha256').update(ticket).digest('hex'), company, 'user-test', Date.now() + 60_000).run();
  const upgrade = () => runtime.dispatchFetch(`https://acme.meshrmm.com/v1/agents/events?protocol=2&token=${ticket}`, {
    headers: { Upgrade: 'websocket', Origin: 'https://acme.meshrmm.com', 'X-Mesh-User-Id': 'forged-user' },
  });
  const apiDashboard = collect(await upgrade());
  await until(() => apiDashboard.messages.length === 2, 'public API event subscription');
  assert.equal((await upgrade()).status, 401, 'subscription token cannot be replayed');
  const apiId = apiDashboard.messages[0].connection_id;
  assert.equal((await post(presence, '/renew', { connection_id: apiId, user_id: 'forged-user' })).status, 410);
  assert.equal((await post(presence, '/renew', { connection_id: apiId, user_id: 'user-test' })).status, 200);

  const first = collect(await connectAgent());
  await until(() => dashboard.messages.some(m => m.type === 'agent_upsert' && m.agent.connected), 'push online');
  assert.equal((await state(agent)).alarm, null, 'stable agent has no recurring alarm');
  assert.equal((await snapshot()).agents[0].connected, true);
  assert.equal((await state(agent)).statusCalls, 0, 'snapshots never contact coordinators');

  const count = dashboard.messages.length;
  assert.equal((await post(presence, '/renew', { connection_id: connectionId, user_id: 'another-user' })).status, 410);
  assert.equal((await post(presence, '/renew', { connection_id: 'unknown', user_id: 'user-test' })).status, 410);
  const renewed = await post(presence, '/renew', { connection_id: connectionId, user_id: 'user-test' });
  assert.equal(renewed.status, 200);
  assert.ok((await renewed.json()).expires_at_unix_ms > Date.now());
  await pause();
  assert.equal(dashboard.messages.length, count, 'renewal does not send another snapshot');

  // Replacement connects before the old close callback. Old callbacks must not
  // take the new connection offline or create an unnecessary presence delta.
  const replacement = collect(await connectAgent());
  first.socket.close();
  await until(async () => (await state(agent)).delivery.acknowledged, 'replacement acknowledged');
  assert.equal((await snapshot()).agents[0].connected, true);
  assert.equal(dashboard.messages.length, count);
  const generation = (await state(agent)).delivery.generation;
  await post(presence, '/presence', { type: 'connection', agent_id: 'device-test', connected: false, generation: generation - 1 });
  assert.equal((await snapshot()).agents[0].connected, true, 'late delivery ignored');

  // A dropped disconnect publication stays in the outbox with a retry alarm.
  await post(presence, '/__test/fail', true);
  replacement.socket.close();
  await until(async () => (await state(agent)).delivery?.acknowledged === false, 'disconnect queued');
  assert.notEqual((await state(agent)).alarm, null);
  await post(presence, '/__test/fail', false);
  assert.equal((await post(agent, '/__test/alarm')).status, 200);
  await until(() => dashboard.messages.some(m => m.type === 'agent_upsert' && !m.agent.connected), 'retried offline delta');
  assert.equal((await snapshot()).agents[0].connected, false);
  assert.equal((await state(agent)).alarm, null);

  // An Agent that stops to install an update shows as updating, not just
  // offline, until it reconnects.
  const updatingAgent = async () => {
    const socket = collect(await connectAgent());
    await until(async () => (await snapshot()).agents[0].connected, 'online before the update');
    socket.socket.send(JSON.stringify({ type: 'updating', version: '9.9.9' }));
    await until(async () => (await state(agent)).pendingUpdate?.version === '9.9.9', 'update recorded');
    socket.socket.close();
    await until(() => dashboard.messages.at(-1)?.agent?.updating_to === '9.9.9', 'push updating');
  };
  await updatingAgent();
  assert.equal(dashboard.messages.at(-1).agent.connected, false);
  assert.equal((await snapshot()).agents[0].updating_to, '9.9.9');
  assert.ok((await state(agent)).alarm > Date.now() + 60_000, 'the update grace period is scheduled');
  const updated = collect(await connectAgent());
  await until(() => dashboard.messages.at(-1)?.agent?.connected === true, 'push online after the update');
  assert.equal(dashboard.messages.at(-1).agent.updating_to, undefined);
  assert.equal((await state(agent)).pendingUpdate, undefined, 'reconnecting ends the update');
  updated.socket.close();
  await until(async () => (await state(agent)).delivery.connected === false && (await state(agent)).delivery.acknowledged, 'ordinary disconnect');
  assert.equal((await snapshot()).agents[0].updating_to, undefined, 'a later outage is not an update');
  // An update that never brings the Agent back shows as offline after the grace period.
  await updatingAgent();
  await post(agent, '/__test/expire-update');
  await post(agent, '/__test/alarm');
  await until(async () => (await snapshot()).agents[0].updating_to === undefined, 'update grace period ended');
  assert.equal(dashboard.messages.at(-1).agent.updating_to, undefined);
  assert.equal((await snapshot()).agents[0].connected, false);
  assert.equal((await state(agent)).alarm, null);
  // Only a release version is accepted, since the dashboard displays it.
  const invalid = collect(await connectAgent());
  const invalidClosed = new Promise(resolve => invalid.socket.addEventListener('close', event => resolve(event.code)));
  invalid.socket.send(JSON.stringify({ type: 'updating', version: '<b>1</b>' }));
  assert.equal(await invalidClosed, 1003);
  await until(async () => (await state(agent)).delivery.connected === false && (await state(agent)).delivery.acknowledged, 'invalid update rejected');
  assert.equal((await snapshot()).agents[0].updating_to, undefined);
  assert.equal((await state(agent)).pendingUpdate, undefined);

  // Deleted devices cannot reappear through a delayed creation or connection event.
  await db.exec("UPDATE agents SET deletion_requested_at = 1 WHERE id = 'device-test'");
  // Simulate a lost HTTP notification: the existing alarm drains the durable outbox.
  await post(presence, '/__test/alarm');
  await until(() => dashboard.messages.some(m => m.type === 'agent_deleted'), 'catalog deletion retried');
  assert.equal((await db.prepare('SELECT COUNT(*) AS n FROM presence_catalog_outbox').first()).n, 0);
  const deletedRevision = (await snapshot()).revision;
  await post(presence, '/presence', { type: 'upsert', agent_id: 'device-test', name: 'Old name', connected: false });
  assert.deepEqual((await snapshot()).agents, []);
  assert.equal((await snapshot()).revision, deletedRevision);

  // Expiry cannot be extended retroactively, and suspension blocks renewal.
  await post(presence, '/__test/expire-subscriptions');
  assert.equal((await post(presence, '/renew', { connection_id: connectionId, user_id: 'user-test' })).status, 410);
  await db.exec("UPDATE companies SET status = 'suspended'");
  assert.equal((await post(presence, '/renew', { connection_id: connectionId, user_id: 'user-test' })).status, 403);
  await post(presence, '/__test/alarm');

  // Activity persists exact deadlines but leaves the existing alarm in place.
  await db.exec("UPDATE companies SET status = 'active'; UPDATE agents SET deletion_requested_at = NULL;");
  const running = collect(await connectAgent());
  const session = sessions.get(sessions.idFromName('session-test'));
  const lease = { session_id: 'session-test', device_id: 'device-test', viewer_name: 'Test', signaling_token: 'agent-token', expires_at_unix_ms: Date.now() + 900_000, ice_servers: [] };
  assert.equal((await post(agent, '/request', lease)).status, 200);
  assert.equal((await post(session, '/init', { ...lease, client_token: 'client-token', agent_token: 'agent-token', idle_timeout_ms: 900_000 })).status, 200);
  const peer = collect(await session.fetch('https://session.internal/signal?role=client', { headers: { Upgrade: 'websocket', Authorization: 'Bearer client-token' } }));
  const before = await state(session);
  peer.socket.send(JSON.stringify({ type: 'activity' }));
  await until(async () => (await state(session)).session.expires_at_unix_ms > before.session.expires_at_unix_ms, 'activity persisted');
  assert.equal((await state(session)).alarm, before.alarm, 'activity does not rewrite alarm');
  await post(session, '/__test/alarm');
  assert.ok((await state(session)).alarm > Date.now(), 'early alarm reschedules to durable deadline');

  // Lease renewals leave the request the Agent received unchanged, so the
  // replay after the Agent reconnects matches the session it is running.
  const delivered = running.messages.find(m => m.session_id === 'session-test');
  assert.ok(delivered, 'session delivered to the Agent');
  assert.equal((await post(agent, '/lease', { ...lease, expires_at_unix_ms: Date.now() + 1_800_000 })).status, 200);
  // A failed online publication does not refuse a reconnecting Agent: the
  // live session is still resumed on the new socket and the alarm retries.
  await post(presence, '/__test/fail', true);
  const unpublished = collect(await connectAgent());
  await until(() => unpublished.messages.some(m => m.session_id === 'session-test'), 'session resumed despite presence failure');
  assert.deepEqual(unpublished.messages.find(m => m.session_id === 'session-test'), delivered, 'replay repeats the delivered request');
  const queued = await state(agent);
  assert.equal(queued.delivery.connected, true);
  assert.equal(queued.delivery.acknowledged, false, 'online publication stays in the outbox');
  assert.notEqual(queued.alarm, null);
  await post(presence, '/__test/fail', false);
  assert.equal((await post(agent, '/__test/alarm')).status, 200);
  assert.equal((await state(agent)).delivery.acknowledged, true, 'alarm delivered the online publication');
  assert.equal((await snapshot()).agents[0].connected, true);

  // A suspension whose revocation never reached the coordinator still takes
  // effect at its next session request: the Agent is disconnected and the
  // live session ends.
  const unrevoked = collect(await connectAgent());
  const closeCode = new Promise(resolve => unrevoked.socket.addEventListener('close', event => resolve(event.code)));
  const peerClosed = new Promise(resolve => peer.socket.addEventListener('close', event => resolve(event.code)));
  await db.exec("UPDATE companies SET status = 'suspended'");
  assert.equal((await post(agent, '/lease', lease)).status, 403);
  assert.equal(await closeCode, 4001);
  assert.equal(await peerClosed, 4001);
  await until(async () => (await state(session)).session === undefined, 'suspended session expired');
  assert.equal((await post(agent, '/request', { ...lease, session_id: 'session-after-suspension' })).status, 403);
  await db.exec("UPDATE companies SET status = 'active'");

  // Suspending keeps revoking past a coordinator that fails.
  await db.exec("ALTER TABLE companies ADD COLUMN updated_at INTEGER; CREATE TABLE platform_audit_events (id TEXT PRIMARY KEY, actor_user_id TEXT, action TEXT, company_id TEXT, metadata_json TEXT, created_at INTEGER); INSERT INTO agents VALUES ('device-failing','company-test','Failing PC',NULL);");
  const connectDevice = (stub, device) => stub.fetch('https://agent.internal/connect', { headers: {
    ...headers, Upgrade: 'websocket', 'X-Mesh-Device-Id': device, 'X-Mesh-Uninstall-Requested': 'false',
  }});
  const failing = agents.get(agents.idFromName('device-failing'));
  const failingAgent = collect(await connectDevice(failing, 'device-failing'));
  await post(failing, '/__test/fail', true);
  const healthy = collect(await connectAgent());
  const healthyClosed = new Promise(resolve => healthy.socket.addEventListener('close', event => resolve(event.code)));
  const suspend = await runtime.dispatchFetch('https://admin.meshrmm.com/v1/platform/companies/company-test/suspend', {
    method: 'POST', headers: { Authorization: `Bearer ${token('owner-test')}` },
  });
  assert.equal(suspend.status, 204, 'one failed coordinator does not fail the suspension');
  assert.equal(await healthyClosed, 4001, 'the other Agents are still revoked');
  assert.equal((await db.prepare("SELECT status FROM companies WHERE id = 'company-test'").first()).status, 'suspended');
  await post(failing, '/__test/fail', false);
  const failingClosed = new Promise(resolve => failingAgent.socket.addEventListener('close', event => resolve(event.code)));
  assert.equal((await post(failing, '/lease', lease)).status, 403);
  assert.equal(await failingClosed, 4001, 'the missed Agent is revoked at its next request');
  await db.exec("UPDATE companies SET status = 'active'");
  console.log('Presence integration passed: event-only snapshots, durable retry, replacement ordering, renewal isolation/expiry/revocation, signing-key caching, retryable auth outages, Agent update status and its grace period, deletion, session alarm preservation, unchanged replays after lease renewal, Agent connect past a presence failure, suspension fan-out past failures, and revocation after a missed suspension.');
} finally {
  for (const socket of sockets) { try { socket.close(); } catch {} }
  await runtime.dispose();
}
