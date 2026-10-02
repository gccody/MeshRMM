// Runs the compiled Worker's toolbox routes inside workerd with a local R2
// bucket: private and shared scripts and files, administrator edits, uploads
// checked against their SHA-256, script runs and file deliveries through a
// connected Agent from the dashboard and from a remote session, and Agent
// reports. Run after worker-build with dashboard npm dependencies installed.
import assert from 'node:assert/strict';
import { createHash, generateKeyPairSync, sign } from 'node:crypto';
import { readdir, readFile } from 'node:fs/promises';
import { fileURLToPath } from 'node:url';
import { Miniflare, convertV4MiniflareOptions } from '../../dashboard/node_modules/miniflare/dist/src/index.js';

const root = fileURLToPath(new URL('../build/', import.meta.url));
const migrations = new URL('../migrations/', import.meta.url);
const { publicKey, privateKey } = generateKeyPairSync('rsa', { modulusLength: 2048 });
const jwk = { ...publicKey.export({ format: 'jwk' }), kid: 'test-key', alg: 'RS256', use: 'sig' };
const token = (user, { organization = 'org-acme', permissions = [] } = {}) => {
  const encode = value => Buffer.from(JSON.stringify(value)).toString('base64url');
  const data = `${encode({ alg: 'RS256', kid: 'test-key' })}.${encode({
    sub: user, org_id: organization, client_id: 'client-test', iss: 'https://api.workos.com',
    exp: Math.floor(Date.now() / 1000) + 300, permissions,
  })}`;
  return `${data}.${sign('RSA-SHA256', Buffer.from(data), privateKey).toString('base64url')}`;
};
const sha256 = value => createHash('sha256').update(value).digest('hex');
const agentToken = 'b'.repeat(64);
const admin = ['company:settings:manage'];

const runtime = new Miniflare(convertV4MiniflareOptions({ workers: [{
  name: 'toolbox-test',
  modules: [
    { type: 'ESModule', path: `${root}index.js` },
    { type: 'CompiledWasm', path: `${root}index_bg.wasm` },
  ],
  compatibilityDate: '2026-08-22', compatibilityFlags: ['nodejs_compat'],
  durableObjects: {
    AGENT_COORDINATOR: { className: 'AgentCoordinator', useSQLite: true },
    COMPANY_PRESENCE: { className: 'CompanyPresence', useSQLite: true },
    REMOTE_SESSION: { className: 'RemoteSession', useSQLite: true },
  },
  d1Databases: ['DB'], r2Buckets: ['THUMBNAILS', 'TOOLBOX'],
  bindings: { PLATFORM_OWNER_USER_IDS: 'owner-test', WORKOS_CLIENT_ID: 'client-test', WORKOS_ISSUER: 'https://api.workos.com', TENANT_ROOT_DOMAIN: 'meshrmm.com', PUBLIC_API_URL: 'https://api.meshrmm.com', DASHBOARD_ORIGIN: 'https://meshrmm.com' },
  outboundService: 'jwks',
}, {
  name: 'jwks', modules: true,
  script: `export default { fetch(request) {
    if (request.url !== 'https://api.workos.com/sso/jwks/client-test') throw new Error('Unexpected external request');
    return Response.json(${JSON.stringify({ keys: [jwk] })}); } };`,
}] }));

try {
  const db = await runtime.getD1Database('DB');
  const bucket = await runtime.getR2Bucket('TOOLBOX');
  for (const file of (await readdir(migrations)).filter(name => name.endsWith('.sql')).sort()) {
    const sql = await readFile(new URL(file, migrations), 'utf8');
    await db.exec(sql.replace(/--[^\n]*/g, '').replace(/\s*\n\s*/g, ' '));
  }
  await db.exec(`INSERT INTO companies (id, name, created_at, slug, status, workos_organization_id) VALUES ('company-acme', 'Acme', 0, 'acme', 'active', 'org-acme'), ('company-other', 'Other', 0, 'other', 'active', 'org-other'); INSERT INTO agents (id, company_id, name, auth_token_hash, created_by_user_id, created_at, updated_at) VALUES ('device-acme', 'company-acme', 'Acme PC', '${sha256(agentToken)}', 'user-ada', 0, 0), ('device-offline', 'company-acme', 'Spare PC', '${sha256(agentToken)}', 'user-ada', 0, 0);`);

  const api = async (path, { user = 'user-ada', organization, permissions, method = 'GET', body, headers = {}, host = 'acme.meshrmm.com' } = {}) => {
    const json = body !== undefined && !(body instanceof Uint8Array);
    const response = await runtime.dispatchFetch(`https://${host}${path}`, {
      method,
      headers: { Authorization: `Bearer ${token(user, { organization, permissions })}`, ...(json ? { 'Content-Type': 'application/json' } : {}), ...headers },
      body: json ? JSON.stringify(body) : body,
    });
    const text = await response.text();
    let data = null;
    try { data = text ? JSON.parse(text) : null; } catch { data = text; }
    return { status: response.status, data };
  };
  const script = (name, extra = {}) => ({ name, folder: 'Maintenance', language: 'powershell', body: 'Get-Date', ...extra });

  // Scripts: private to their owner unless shared.
  let result = await api('/v1/toolbox/scripts', { method: 'POST', body: script('Ada private') });
  assert.equal(result.status, 201, JSON.stringify(result.data));
  const privateScript = result.data;
  assert.deepEqual([privateScript.shared, privateScript.owned, privateScript.can_edit, privateScript.timeout_seconds, privateScript.body], [false, true, true, 300, 'Get-Date']);
  result = await api('/v1/toolbox/scripts', { method: 'POST', body: script(' Ada shared ', { shared: true, folder: ' Disk / Cleanup ', language: 'cmd', body: 'dir', timeout_seconds: 60 }) });
  assert.equal(result.status, 201);
  const sharedScript = result.data;
  assert.deepEqual([sharedScript.name, sharedScript.folder], ['Ada shared', 'Disk/Cleanup'], 'names and folders are normalized');
  for (const invalid of [script(''), script('x', { body: '  ' }), script('x', { timeout_seconds: 5 }), script('x', { language: 'bash' }), script('x', { folder: 'a/'.repeat(9) })]) {
    assert.equal((await api('/v1/toolbox/scripts', { method: 'POST', body: invalid })).status, 400, JSON.stringify(invalid));
  }

  let listing = (await api('/v1/toolbox')).data;
  assert.deepEqual(listing.scripts.map(item => item.name).sort(), ['Ada private', 'Ada shared']);
  assert.equal(listing.scripts[0].body, undefined, 'lists leave out script bodies');
  listing = (await api('/v1/toolbox', { user: 'user-bob' })).data;
  assert.deepEqual(listing.scripts.map(item => [item.name, item.owned, item.can_edit]), [['Ada shared', false, false]], "another member sees only what is shared");
  assert.equal((await api(`/v1/toolbox/scripts/${privateScript.id}`, { user: 'user-bob' })).status, 404);
  assert.equal((await api('/v1/toolbox', { organization: 'org-other', host: 'other.meshrmm.com' })).data.scripts.length, 0, 'other companies see nothing');

  assert.equal((await api(`/v1/toolbox/scripts/${sharedScript.id}`, { user: 'user-bob', method: 'PUT', body: script('Bob edit', { shared: true }) })).status, 404, 'a member cannot change a shared script');
  result = await api(`/v1/toolbox/scripts/${sharedScript.id}`, { user: 'user-bob', permissions: admin, method: 'PUT', body: script('Admin edit', { shared: true, body: 'Get-Volume' }) });
  assert.equal(result.status, 200, 'an administrator can');
  assert.deepEqual([result.data.name, result.data.body, result.data.owned], ['Admin edit', 'Get-Volume', false]);
  assert.equal((await api(`/v1/toolbox/scripts/${privateScript.id}`, { user: 'user-bob', permissions: admin, method: 'DELETE' })).status, 404, 'private scripts stay private, even from administrators');

  // Files: uploaded with their SHA-256, which R2 checks.
  const content = Buffer.from('installer bytes');
  const upload = (name, body, { digest = sha256(body), shared = false, user } = {}) => api(
    `/v1/toolbox/files?${new URLSearchParams({ name, folder: 'Installers', shared: String(shared), sha256: digest })}`,
    // Browsers send a file's length; Miniflare's dispatchFetch does not.
    { method: 'POST', body: new Uint8Array(body), user, headers: { 'Content-Length': String(body.length) } },
  );
  assert.equal((await upload('setup.exe', content, { digest: sha256('something else') })).status, 400, 'a corrupted upload is refused');
  assert.equal((await upload('bad:name.exe', content)).status, 400);
  assert.equal((await upload('CON.txt', content)).status, 400);
  assert.equal((await bucket.list()).objects.length, 0, 'refused uploads store nothing');
  result = await upload('setup.exe', content, { shared: true });
  assert.equal(result.status, 201, JSON.stringify(result.data));
  const sharedFile = result.data;
  assert.deepEqual([sharedFile.name, sharedFile.folder, sharedFile.size_bytes, sharedFile.sha256, sharedFile.shared], ['setup.exe', 'Installers', content.length, sha256(content), true]);
  result = await upload('empty.txt', Buffer.alloc(0), { user: 'user-bob' });
  assert.equal(result.status, 201, 'empty files upload too');
  const bobFile = result.data;
  assert.ok(await bucket.get(`toolbox/company-acme/${sharedFile.id}`), 'content is stored under its company');

  const download = await runtime.dispatchFetch(`https://acme.meshrmm.com/v1/toolbox/files/${sharedFile.id}/content`, { headers: { Authorization: `Bearer ${token('user-bob')}` } });
  assert.equal(download.status, 200);
  assert.deepEqual(Buffer.from(await download.arrayBuffer()), content);
  assert.equal((await api(`/v1/toolbox/files/${bobFile.id}/content`)).status, 404, "another member's private file is hidden");
  result = await api(`/v1/toolbox/files/${sharedFile.id}`, { method: 'PUT', body: { name: 'setup-v2.exe', folder: 'Installers/Chrome', shared: true } });
  assert.deepEqual([result.status, result.data.name, result.data.folder], [200, 'setup-v2.exe', 'Installers/Chrome']);

  // Runs and deliveries need a connected Agent.
  result = await api('/v1/agents/device-offline/script-runs', { method: 'POST', body: { script_id: privateScript.id, run_as: 'system' } });
  assert.equal(result.status, 409, 'an offline device runs nothing');
  let runs = (await api('/v1/script-runs?device_id=device-offline')).data.runs;
  assert.deepEqual(runs.map(run => [run.status, run.error]), [['failed', 'The device is offline, so the script did not run.']]);
  assert.equal((await api('/v1/agents/device-acme/script-runs', { user: 'user-bob', method: 'POST', body: { script_id: privateScript.id, run_as: 'system' } })).status, 404, "a member cannot run another's private script");

  const connection = await runtime.dispatchFetch('https://acme.meshrmm.com/v1/agents/device-acme/connect', { headers: { Upgrade: 'websocket', Authorization: `Bearer ${agentToken}` } });
  assert.equal(connection.status, 101);
  const socket = connection.webSocket;
  socket.accept();
  const commands = [];
  socket.addEventListener('message', event => { if (event.data !== 'pong') commands.push(JSON.parse(event.data)); });
  const nextCommand = async type => {
    for (let attempt = 0; attempt < 100; attempt += 1) {
      const index = commands.findIndex(command => command.type === type);
      if (index >= 0) return commands.splice(index, 1)[0];
      await new Promise(resolve => setTimeout(resolve, 20));
    }
    throw new Error(`no ${type} command: ${JSON.stringify(commands)}`);
  };
  const agent = (path, { method = 'POST', body } = {}) => runtime.dispatchFetch(`https://acme.meshrmm.com/v1/agents/device-acme${path}`, {
    method, headers: { Authorization: `Bearer ${agentToken}`, ...(body ? { 'Content-Type': 'application/json' } : {}) }, body: body && JSON.stringify(body),
  });

  result = await api('/v1/agents/device-acme/script-runs', { method: 'POST', body: { script_id: privateScript.id, run_as: 'user' } });
  assert.equal(result.status, 201, JSON.stringify(result.data));
  const run = result.data;
  assert.deepEqual([run.status, run.run_as, run.script_name, run.source, run.requested_by_you], ['pending', 'user', 'Ada private', 'dashboard', true]);
  const runCommand = await nextCommand('run_script');
  assert.deepEqual(runCommand.run, { run_id: run.id, language: 'powershell', body: 'Get-Date', run_as: 'user', timeout_seconds: 300 });
  assert.equal((await agent(`/script-runs/${run.id}/result`, { body: { status: 'pending', ran_as: 'x' } })).status, 400, 'a report says how the run finished');
  let response = await agent(`/script-runs/${run.id}/result`, { body: { status: 'completed', ran_as: 'ACME\\ada', exit_code: 0, stdout: 'Thursday', stderr: 'warning' } });
  assert.equal(response.status, 204, await response.text());
  assert.equal((await agent(`/script-runs/${run.id}/result`, { body: { status: 'failed', ran_as: 'SYSTEM' } })).status, 404, 'a run is reported once');
  result = await api(`/v1/script-runs/${run.id}`);
  assert.deepEqual([result.data.status, result.data.ran_as, result.data.exit_code, result.data.stdout, result.data.stderr], ['completed', 'ACME\\ada', 0, 'Thursday', 'warning']);
  assert.equal((await api(`/v1/script-runs/${run.id}`, { user: 'user-bob' })).status, 404, "members cannot read others' runs");
  assert.equal((await api(`/v1/script-runs/${run.id}`, { user: 'user-bob', permissions: admin })).status, 200, 'administrators can');
  runs = (await api('/v1/script-runs')).data.runs;
  assert.equal(runs[0].id, run.id);
  assert.equal(runs[0].stdout, '', 'lists leave out output');

  // A remote session's viewer uses its technician's toolbox with the session token.
  const sessions = await runtime.getDurableObjectNamespace('REMOTE_SESSION');
  const sessionId = crypto.randomUUID();
  const session = sessions.get(sessions.idFromName(sessionId));
  const init = await session.fetch('https://session.internal/init', {
    method: 'POST', headers: { 'Content-Type': 'application/json' },
    body: JSON.stringify({ session_id: sessionId, device_id: 'device-acme', company_id: 'company-acme', user_id: 'user-bob', viewer_name: 'Bob', client_token: 'client-token', agent_token: 'agent-token', expires_at_unix_ms: Date.now() + 900_000, idle_timeout_ms: 900_000 }),
  });
  assert.equal(init.status, 200, await init.text());
  const viewer = async (path, { method = 'GET', body, bearer = 'client-token' } = {}) => {
    const response = await runtime.dispatchFetch(`https://acme.meshrmm.com/v1/remote/sessions/${sessionId}${path}`, {
      method, headers: { Authorization: `Bearer ${bearer}`, ...(body ? { 'Content-Type': 'application/json' } : {}) }, body: body && JSON.stringify(body),
    });
    const text = await response.text();
    return { status: response.status, data: text ? JSON.parse(text) : null };
  };
  assert.equal((await viewer('/toolbox', { bearer: 'agent-token' })).status, 401, 'only the viewer may use the toolbox');
  result = await viewer('/toolbox');
  assert.equal(result.status, 200);
  assert.deepEqual(result.data.scripts.map(item => [item.name, item.folder, item.language, item.shared]), [['Admin edit', 'Maintenance', 'powershell', true]], "the session's technician sees their toolbox");
  assert.deepEqual(result.data.files.map(item => item.name).sort(), ['empty.txt', 'setup-v2.exe']);
  assert.equal((await viewer('/script-runs', { method: 'POST', body: { script_id: privateScript.id, run_as: 'system' } })).status, 404);
  result = await viewer('/script-runs', { method: 'POST', body: { script_id: sharedScript.id, run_as: 'system' } });
  assert.equal(result.status, 201, JSON.stringify(result.data));
  const sessionRun = result.data;
  assert.equal((await nextCommand('run_script')).run.body, 'Get-Volume');
  assert.equal((await viewer(`/script-runs/${sessionRun.id}`)).data.status, 'pending');
  assert.equal((await viewer(`/script-runs/${run.id}`)).status, 404, "the viewer reads only its technician's runs");
  assert.equal((await agent(`/script-runs/${sessionRun.id}/result`, { body: { status: 'timed_out', ran_as: 'SYSTEM', stdout: 'é'.repeat(300_000), error: 'stopped after 60 seconds' } })).status, 204);
  result = await viewer(`/script-runs/${sessionRun.id}`);
  assert.deepEqual([result.data.status, result.data.output_truncated, Buffer.byteLength(result.data.stdout)], ['timed_out', true, 512 * 1024], 'output is cut at its limit');

  result = await viewer('/file-deliveries', { method: 'POST', body: { file_id: sharedFile.id, background: true } });
  assert.equal(result.status, 201, JSON.stringify(result.data));
  const delivery = result.data;
  const deliverCommand = await nextCommand('deliver_file');
  assert.deepEqual(deliverCommand.delivery, { delivery_id: delivery.id, file_name: 'setup-v2.exe', size_bytes: content.length, sha256: sha256(content), destination: 'public' });
  response = await agent(`/file-deliveries/${delivery.id}/content`, { method: 'GET' });
  assert.equal(response.status, 200);
  assert.deepEqual(Buffer.from(await response.arrayBuffer()), content, 'the Agent downloads the file');
  assert.equal((await runtime.dispatchFetch(`https://acme.meshrmm.com/v1/agents/device-offline/file-deliveries/${delivery.id}/content`, { headers: { Authorization: `Bearer ${agentToken}` } })).status, 404, "another device cannot download it");
  assert.equal((await agent(`/file-deliveries/${delivery.id}/result`, { body: { status: 'delivered', path: 'C:\\Users\\Public\\Documents\\MeshRMM Transferred Files\\setup-v2.exe' } })).status, 204);
  result = await viewer(`/file-deliveries/${delivery.id}`);
  assert.deepEqual([result.data.status, result.data.path], ['delivered', 'C:\\Users\\Public\\Documents\\MeshRMM Transferred Files\\setup-v2.exe']);
  assert.equal((await agent(`/file-deliveries/${delivery.id}/content`, { method: 'GET' })).status, 404, 'a finished delivery cannot be downloaded again');
  result = await viewer('/file-deliveries', { method: 'POST', body: { file_id: bobFile.id } });
  assert.equal(result.status, 201);
  assert.equal((await nextCommand('deliver_file')).delivery.destination, 'user');

  const audit = await db.prepare("SELECT action FROM audit_events WHERE company_id = 'company-acme' ORDER BY created_at").all();
  for (const action of ['toolbox.script_create', 'toolbox.script_update', 'toolbox.file_upload', 'toolbox.file_update', 'script.run', 'file.deliver']) {
    assert.ok(audit.results.some(row => row.action === action), `${action} is audited`);
  }

  // Deleting removes the content too; the session ends with its record.
  assert.equal((await api(`/v1/toolbox/files/${sharedFile.id}`, { method: 'DELETE' })).status, 204);
  assert.equal(await bucket.get(`toolbox/company-acme/${sharedFile.id}`), null);
  assert.equal((await api(`/v1/toolbox/scripts/${sharedScript.id}`, { method: 'DELETE' })).status, 204);
  assert.equal((await api('/v1/script-runs')).data.runs.find(item => item.id === run.id).script_name, 'Ada private', 'runs keep their script name');
  await session.fetch('https://session.internal/end', { method: 'POST', headers: { Authorization: 'Bearer client-token' } });
  assert.equal((await viewer('/toolbox')).status, 410, 'an ended session has no toolbox');
  socket.close();
  console.log('Toolbox passed: private and shared items, administrator edits, checked uploads, runs and deliveries from the dashboard and a session, and Agent reports.');
} finally {
  await runtime.dispose();
}
