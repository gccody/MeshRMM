// Rotates an Agent credential through the public route and the compiled
// coordinator inside workerd. A test-only wrapper exposes coordinator storage;
// it is never deployed.
import assert from 'node:assert/strict';
import { createHash, generateKeyPairSync, sign } from 'node:crypto';
import { readdir, readFile } from 'node:fs/promises';
import { fileURLToPath } from 'node:url';
import { Miniflare, convertV4MiniflareOptions } from '../../dashboard/node_modules/miniflare/dist/src/index.js';

const root = fileURLToPath(new URL('../build/', import.meta.url));
const migrations = new URL('../migrations/', import.meta.url);
const { publicKey, privateKey } = generateKeyPairSync('rsa', { modulusLength: 2048 });
const jwk = { ...publicKey.export({ format: 'jwk' }), kid: 'test-key', alg: 'RS256', use: 'sig' };
const adminToken = () => {
  const encode = value => Buffer.from(JSON.stringify(value)).toString('base64url');
  const data = `${encode({ alg: 'RS256', kid: 'test-key' })}.${encode({
    sub: 'user-test', org_id: 'org-test', client_id: 'client-test', iss: 'https://api.workos.com',
    exp: Math.floor(Date.now() / 1000) + 300, permissions: ['agents:manage'],
  })}`;
  return `${data}.${sign('RSA-SHA256', Buffer.from(data), privateKey).toString('base64url')}`;
};
const hash = value => createHash('sha256').update(value).digest('hex');
const wrapper = `
import Server, { AgentCoordinator as Agent, CompanyPresence, RemoteSession } from './index.js';
export class AgentCoordinator {
  constructor(ctx, env) { this.ctx = ctx; this.inner = new Agent(ctx, env); }
  async fetch(request) {
    const path = new URL(request.url).pathname;
    if (path === '/__test/rotation') return Response.json({ rotation: await this.ctx.storage.get('pending_rotation') ?? null });
    return this.inner.fetch(request);
  }
  alarm() { return this.inner.alarm(); }
  webSocketMessage(...args) { return this.inner.webSocketMessage(...args); }
  webSocketClose(...args) { return this.inner.webSocketClose(...args); }
  webSocketError(...args) { return this.inner.webSocketError(...args); }
}
export { CompanyPresence, RemoteSession };
export default Server;
`;
const runtime = new Miniflare(convertV4MiniflareOptions({ workers: [{
  name: 'rotation-test',
  modules: [
    { type: 'ESModule', path: `${root}rotation-test.js`, contents: wrapper },
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

const pause = () => new Promise(resolve => setTimeout(resolve, 10));
async function until(predicate, message) {
  for (let i = 0; i < 200; i++) { if (await predicate()) return; await pause(); }
  assert.fail(message);
}
const sockets = [];
try {
  const db = await runtime.getD1Database('DB');
  for (const file of (await readdir(migrations)).filter(name => name.endsWith('.sql')).sort()) {
    const sql = await readFile(new URL(file, migrations), 'utf8');
    await db.exec(sql.replace(/--[^\n]*/g, '').replace(/\s*\n\s*/g, ' '));
  }
  const original = 'a'.repeat(64);
  await db.exec(`INSERT INTO companies (id, name, created_at, slug, status, workos_organization_id) VALUES ('company-test', 'Acme', 0, 'acme', 'active', 'org-test'); INSERT INTO agents (id, company_id, name, auth_token_hash, created_by_user_id, created_at, updated_at) VALUES ('device-test', 'company-test', 'Test PC', '${hash(original)}', 'user-test', 0, 0);`);
  const agents = await runtime.getDurableObjectNamespace('AGENT_COORDINATOR');
  const coordinator = agents.get(agents.idFromName('device-test'));
  const stagedRotation = async () => (await (await coordinator.fetch('https://test.internal/__test/rotation')).json()).rotation;
  const credentials = () => db.prepare("SELECT auth_token_hash AS current, pending_auth_token_hash AS pending FROM agents WHERE id = 'device-test'").first();
  const rotate = () => runtime.dispatchFetch('https://acme.meshrmm.com/v1/agents/device-test/rotate-token', {
    method: 'POST', headers: { Authorization: `Bearer ${adminToken()}` },
  });
  const connect = async agentToken => {
    const response = await runtime.dispatchFetch('https://acme.meshrmm.com/v1/agents/device-test/connect', {
      headers: { Upgrade: 'websocket', Authorization: `Bearer ${agentToken}` },
    });
    assert.equal(response.status, 101);
    const socket = response.webSocket;
    const rotations = [];
    socket.addEventListener('message', event => {
      const message = JSON.parse(event.data);
      if (message.type === 'rotate_token') rotations.push(message.token);
    });
    socket.accept(); sockets.push(socket);
    return { socket, rotations };
  };

  // An offline Agent cannot receive a credential, so nothing is staged.
  let response = await rotate();
  assert.equal(response.status, 409);
  assert.match((await response.json()).error, /must be online/);
  assert.deepEqual(await credentials(), { current: hash(original), pending: null });
  assert.equal(await stagedRotation(), null);

  // An online Agent receives a credential; the current one stays valid.
  const first = await connect(original);
  response = await rotate();
  assert.equal(response.status, 202);
  await until(() => first.rotations.length === 1, 'rotated credential sent');
  const rotated = first.rotations[0];
  assert.match(rotated, /^[0-9a-f]{64}$/);
  assert.deepEqual(await credentials(), { current: hash(original), pending: hash(rotated) });

  // Rotating while the credential is pending resends it rather than failing
  // or replacing a credential the Agent may already have saved.
  assert.equal((await rotate()).status, 202);
  await until(() => first.rotations.length === 2, 'pending credential resent');
  assert.equal(first.rotations[1], rotated);
  assert.equal((await credentials()).pending, hash(rotated));
  const audit = await db.prepare("SELECT metadata_json FROM audit_events WHERE action = 'agent.rotate_token' ORDER BY created_at, rowid").all();
  assert.deepEqual(audit.results.map(row => JSON.parse(row.metadata_json).redelivered), [false, true]);

  // An Agent that reconnects with its old credential gets the pending one again.
  first.socket.close();
  const stillOld = await connect(original);
  await until(() => stillOld.rotations.length === 1, 'pending credential sent on connect');
  assert.equal(stillOld.rotations[0], rotated);

  // Authenticating with the new credential promotes it and deletes the
  // plaintext copy; later connections are not sent it again.
  stillOld.socket.close();
  const promoted = await connect(rotated);
  await until(async () => (await stagedRotation()) === null, 'plaintext credential deleted');
  assert.deepEqual(await credentials(), { current: hash(rotated), pending: null });
  assert.equal((await runtime.dispatchFetch('https://acme.meshrmm.com/v1/agents/device-test/connect', {
    headers: { Upgrade: 'websocket', Authorization: `Bearer ${original}` },
  })).status, 401, 'the replaced credential no longer authenticates');
  await pause();
  assert.deepEqual(promoted.rotations, []);

  // A pending hash whose credential was never sent, such as one left by an
  // earlier failure, no longer blocks rotation: it is replaced.
  await db.exec(`UPDATE agents SET pending_auth_token_hash = '${'f'.repeat(64)}' WHERE id = 'device-test'`);
  assert.equal((await rotate()).status, 202);
  await until(() => promoted.rotations.length === 1, 'replacement credential sent');
  assert.equal((await credentials()).pending, hash(promoted.rotations[0]));

  // A staged credential that D1 no longer expects is never sent: an Agent
  // that adopted it would be locked out.
  await db.exec("UPDATE agents SET pending_auth_token_hash = NULL WHERE id = 'device-test'");
  promoted.socket.close();
  const afterWithdrawal = await connect(rotated);
  await until(async () => (await stagedRotation()) === null, 'withdrawn credential deleted');
  await pause();
  assert.deepEqual(afterWithdrawal.rotations, []);

  // Another company cannot rotate or probe this Agent.
  await db.exec("UPDATE companies SET workos_organization_id = 'org-acme' WHERE id = 'company-test'");
  await db.exec("INSERT INTO companies (id, name, created_at, slug, status, workos_organization_id) VALUES ('company-other', 'Other', 0, 'other', 'active', 'org-test')");
  response = await runtime.dispatchFetch('https://other.meshrmm.com/v1/agents/device-test/rotate-token', {
    method: 'POST', headers: { Authorization: `Bearer ${adminToken()}` },
  });
  assert.equal(response.status, 404);
  assert.deepEqual(await credentials(), { current: hash(rotated), pending: null });
  console.log('token rotation tests passed');
} finally {
  for (const socket of sockets) { try { socket.close(); } catch {} }
  await runtime.dispose();
}
