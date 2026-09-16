// Run after worker-build with dashboard npm dependencies installed.
import assert from 'node:assert/strict';
import { fileURLToPath } from 'node:url';
import { Miniflare, convertV4MiniflareOptions } from '../../dashboard/node_modules/miniflare/dist/src/index.js';

const root = fileURLToPath(new URL('../build/', import.meta.url));
const runtime = new Miniflare(convertV4MiniflareOptions({ workers: [{
  name: "server",
  modules: [
    { type: 'ESModule', path: `${root}index.js` },
    { type: 'CompiledWasm', path: `${root}index_bg.wasm` },
  ],
  compatibilityDate: '2026-08-22',
  compatibilityFlags: ['nodejs_compat'],
  durableObjects: {
    AGENT_COORDINATOR: { className: 'AgentCoordinator', useSQLite: true },
    REMOTE_SESSION: { className: 'RemoteSession', useSQLite: true },
    COMPANY_PRESENCE: { className: 'CompanyPresence', useSQLite: true },
  },
  d1Databases: ['DB'],
}] }));
const post = (stub, path, body) => stub.fetch(`https://test.internal${path}`, {
  method: 'POST', headers: { 'Content-Type': 'application/json' },
  body: body === undefined ? undefined : JSON.stringify(body),
});
try {
  const db = await runtime.getD1Database('DB');
  await db.exec("CREATE TABLE companies (id TEXT, status TEXT); CREATE TABLE agents (id TEXT, company_id TEXT, deletion_requested_at INTEGER); INSERT INTO companies VALUES ('company-test','active'); INSERT INTO agents VALUES ('device-test','company-test',NULL);");
  const agents = await runtime.getDurableObjectNamespace('AGENT_COORDINATOR');
  const sessions = await runtime.getDurableObjectNamespace('REMOTE_SESSION');
  const agent = agents.get(agents.idFromName('device-test'));
  assert.deepEqual(await (await post(agent, '/close-session')).json(), { closed: false });
  // Bypass presence publication in this isolated test; never touches a real Agent.
  const connection = await agent.fetch('https://agent.internal/connect', { headers: {
    Upgrade: 'websocket', 'X-Mesh-Company-Id': 'company-test',
    'X-Mesh-Device-Id': 'device-test', 'X-Mesh-Uninstall-Requested': 'true',
  }});
  assert.equal(connection.status, 101);
  const socket = connection.webSocket;
  socket.accept();
  const commands = [];
  socket.addEventListener('message', event => commands.push(JSON.parse(event.data)));
  const request = id => ({ session_id: id, viewer_name: 'Test viewer', signaling_token: 'agent-token', expires_at_unix_ms: Date.now() + 900_000, ice_servers: [] });
  const first = sessions.get(sessions.idFromName('session-first'));
  assert.equal((await post(first, '/init', { ...request('session-first'), device_id: 'device-test', client_token: 'client-token', agent_token: 'agent-token', idle_timeout_ms: 900_000 })).status, 200);
  assert.equal((await post(agent, '/request', request('session-first'))).status, 200);
  assert.equal((await post(agent, '/request', request('blocked'))).status, 409);
  // Cleanup is independently authenticated and works without a signaling socket.
  assert.equal((await post(first, '/end')).status, 401);
  assert.equal((await first.fetch('https://session.internal/end', { method: 'POST', headers: { Authorization: 'Bearer agent-token' } })).status, 401);
  assert.equal((await post(agent, '/request', request('unauthorized-close-did-not-release'))).status, 409);
  const end = () => first.fetch('https://session.internal/end', { method: 'POST', headers: { Authorization: 'Bearer client-token' } });
  assert.equal((await end()).status, 200);
  assert.equal((await end()).status, 200);
  assert.deepEqual(await (await post(agent, '/close-session')).json(), { closed: false });
  assert.equal((await first.fetch('https://session.internal/resume', { method: 'POST', headers: { Authorization: 'Bearer client-token' } })).status, 410);
  assert.deepEqual(await (await agent.fetch('https://agent.internal/status')).json(), { connected: true });
  // A stale lease with no RemoteSession record must also be recoverable.
  assert.equal((await post(agent, '/request', request('session-orphan'))).status, 200);
  // Late cleanup from the old session cannot release the replacement lease.
  assert.equal((await post(agent, '/session-ended', { session_id: 'session-first' })).status, 200);
  assert.equal((await post(agent, '/request', request('still-blocked'))).status, 409);
  assert.deepEqual(await (await post(agent, '/close-session')).json(), { closed: true });
  assert.deepEqual(await (await post(agent, '/close-session')).json(), { closed: false });
  assert.equal((await post(agent, '/request', request('session-new'))).status, 200);
  await post(agent, '/close-session');
  await new Promise(resolve => setTimeout(resolve, 30));
  assert.ok(commands.some(command => command.type === 'end_session' && command.session_id === 'session-first'), JSON.stringify(commands));
  socket.close();
  console.log('Session cleanup: stale lease recovery, token revocation, repeated close, replacement protection, and Agent notification passed.');
} finally {
  await runtime.dispose();
}
