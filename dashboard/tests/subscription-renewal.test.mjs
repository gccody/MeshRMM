import assert from 'node:assert/strict';
import test from 'node:test';
import { subscriptionRenewal } from '../features/agents/subscription-renewal.ts';

const id = 'a'.repeat(64);
const auth = () => ({ type: 'authorization', connection_id: id, expires_at_unix_ms: Date.now() + 300_000 });
const flush = async () => { for (let i = 0; i < 10; i++) await Promise.resolve(); };

test('renews the same connection before expiry without loading devices or creating a token', async t => {
  t.mock.timers.enable({ apis: ['setTimeout', 'Date'], now: 1_000_000 });
  const requests = [];
  const renewal = subscriptionRenewal(async (path, init) => {
    requests.push({ path, init });
    return Response.json({ expires_at_unix_ms: Date.now() + 300_000 });
  }, () => assert.fail('unexpected disconnect'));
  assert.equal(renewal.accept({ type: 'snapshot' }), false);
  assert.equal(renewal.accept(auth()), true);
  t.mock.timers.tick(239_999);
  assert.equal(requests.length, 0);
  t.mock.timers.tick(1);
  await flush();
  assert.equal(requests.length, 1);
  assert.equal(requests[0].path, '/v1/agents/events/subscriptions/renew');
  assert.deepEqual(JSON.parse(requests[0].init.body), { connection_id: id });
  t.mock.timers.tick(240_000);
  await flush();
  assert.equal(requests.length, 2);
  renewal.stop();
  t.mock.timers.tick(900_000);
  assert.equal(requests.length, 2);
});

test('failed or expired authorization closes the stream instead of retaining access', async t => {
  t.mock.timers.enable({ apis: ['setTimeout', 'Date'], now: 1_000_000 });
  for (const response of [new Response('', { status: 401 }), new Response('', { status: 503 }), Response.json({ expires_at_unix_ms: 1 })]) {
    let disconnected = 0;
    const renewal = subscriptionRenewal(async () => response, () => disconnected++);
    renewal.accept(auth());
    t.mock.timers.tick(240_000);
    await flush();
    assert.equal(disconnected, 1);
    renewal.stop();
  }
});

test('unmount aborts renewal and late completion cannot schedule another request', async t => {
  t.mock.timers.enable({ apis: ['setTimeout', 'Date'], now: 1_000_000 });
  let resolve;
  let signal;
  let calls = 0;
  const renewal = subscriptionRenewal((_path, init) => {
    calls++;
    signal = init.signal;
    return new Promise(done => { resolve = done; });
  }, () => assert.fail('disposed subscription disconnected'));
  renewal.accept(auth());
  t.mock.timers.tick(240_000);
  renewal.stop();
  assert.equal(signal.aborted, true);
  resolve(Response.json({ expires_at_unix_ms: Date.now() + 300_000 }));
  await flush();
  t.mock.timers.tick(900_000);
  assert.equal(calls, 1);
});
