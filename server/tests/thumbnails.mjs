// Runs the compiled Worker's screen thumbnail routes inside workerd with a
// local R2 bucket: Agent uploads, dashboard reads with revalidation, company
// isolation, and removal with the device.
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
const agentToken = 'b'.repeat(64);

const runtime = new Miniflare(convertV4MiniflareOptions({ workers: [{
  name: 'thumbnails-test',
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
  d1Databases: ['DB'], r2Buckets: ['THUMBNAILS'],
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
  const bucket = await runtime.getR2Bucket('THUMBNAILS');
  for (const file of (await readdir(migrations)).filter(name => name.endsWith('.sql')).sort()) {
    const sql = await readFile(new URL(file, migrations), 'utf8');
    await db.exec(sql.replace(/--[^\n]*/g, '').replace(/\s*\n\s*/g, ' '));
  }
  await db.exec(`INSERT INTO companies (id, name, created_at, slug, status, workos_organization_id) VALUES ('company-acme', 'Acme', 0, 'acme', 'active', 'org-acme'), ('company-other', 'Other', 0, 'other', 'active', 'org-other'); INSERT INTO agents (id, company_id, name, auth_token_hash, created_by_user_id, created_at, updated_at, deletion_requested_at) VALUES ('device-acme', 'company-acme', 'Acme PC', '${hash(agentToken)}', 'user-test', 0, 0, NULL), ('device-deleted', 'company-acme', 'Old PC', '${hash(agentToken)}', 'user-test', 0, 0, 1);`);

  const jpeg = (fill, length = 2048) => { const bytes = Buffer.alloc(length, fill); bytes.set([0xff, 0xd8, 0xff, 0xe0]); return bytes; };
  const upload = async (device, body, bearer = agentToken, host = 'acme.meshrmm.com') => {
    const response = await runtime.dispatchFetch(`https://${host}/v1/agents/${device}/thumbnail`, {
      method: 'PUT', headers: { Authorization: `Bearer ${bearer}`, 'Content-Type': 'image/jpeg' }, body,
    });
    return { status: response.status, body: await response.text() };
  };
  const read = async (device, organization = 'org-acme', headers = {}, host = `${organization.slice(4)}.meshrmm.com`) => {
    const response = await runtime.dispatchFetch(`https://${host}/v1/agents/${device}/thumbnail`, {
      headers: { Authorization: `Bearer ${token(organization)}`, ...headers },
    });
    return { status: response.status, headers: response.headers, body: Buffer.from(await response.arrayBuffer()) };
  };
  const key = 'thumbnails/company-acme/device-acme.jpg';

  assert.equal((await read('device-acme')).status, 204, 'no thumbnail before the first upload');
  assert.equal((await upload('device-acme', jpeg(1), 'c'.repeat(64))).status, 401, 'a wrong Agent credential is rejected');
  assert.equal((await upload('device-acme', jpeg(1), agentToken, 'other.meshrmm.com')).status, 401, "another company's hostname is rejected");
  assert.equal((await upload('device-acme', Buffer.from('not a jpeg'))).status, 400);
  assert.equal((await upload('device-acme', jpeg(1, 512 * 1024 + 1))).status, 413);
  assert.equal((await upload('device-deleted', jpeg(1))).status, 409, 'a device being removed cannot upload');
  assert.equal((await bucket.list()).objects.length, 0, 'rejected uploads store nothing');

  let result = await upload('device-acme', jpeg(1));
  assert.equal(result.status, 204, result.body);
  const stored = await bucket.get(key);
  assert.ok(stored, 'the image is stored under its company');
  assert.equal(stored.httpMetadata.contentType, 'image/jpeg');
  assert.deepEqual(Buffer.from(await stored.arrayBuffer()), jpeg(1));

  let first = await read('device-acme');
  assert.equal(first.status, 200);
  assert.deepEqual(first.body, jpeg(1));
  assert.equal(first.headers.get('content-type'), 'image/jpeg');
  assert.equal(first.headers.get('cache-control'), 'private, no-cache');
  assert.equal(first.headers.get('x-content-type-options'), 'nosniff');
  const etag = first.headers.get('etag');
  assert.match(etag, /^".+"$/);
  assert.ok(Date.parse(first.headers.get('last-modified')) > Date.now() - 60_000, 'Last-Modified is the upload time');

  result = await read('device-acme', 'org-acme', { 'If-None-Match': etag });
  assert.equal(result.status, 304, 'an unchanged image is not sent again');
  assert.equal(result.body.length, 0);
  assert.equal(result.headers.get('etag'), etag);
  assert.equal((await read('device-acme', 'org-acme', { 'If-None-Match': '"stale"' })).status, 200);

  assert.equal((await upload('device-acme', jpeg(2))).status, 204);
  result = await read('device-acme', 'org-acme', { 'If-None-Match': etag });
  assert.equal(result.status, 200, 'a new image replaces the old one');
  assert.deepEqual(result.body, jpeg(2));
  assert.notEqual(result.headers.get('etag'), etag);
  assert.equal((await bucket.list()).objects.length, 1, 'only the latest image is kept');

  result = await read('device-acme', 'org-other');
  assert.deepEqual([result.status, result.body.length], [204, 0], "another company cannot read the image");
  assert.equal((await read('device-acme', 'org-other', {}, 'acme.meshrmm.com')).status, 403, "another company's user is refused on this hostname");

  const deleted = await runtime.dispatchFetch('https://acme.meshrmm.com/v1/agents/device-acme', {
    method: 'DELETE', headers: { Authorization: `Bearer ${token('org-acme', ['agents:manage'])}` },
  });
  assert.equal(deleted.status, 204, await deleted.text());
  assert.equal(await bucket.get(key), null, 'deleting the device removes its image');
  assert.equal((await read('device-acme')).status, 204);
  assert.equal((await upload('device-acme', jpeg(3))).status, 409, 'the deleted device cannot put its image back');
  console.log('Thumbnails passed: Agent upload with validation, company-scoped reads, 304 revalidation, replacement, and removal with the device.');
} finally {
  await runtime.dispose();
}
