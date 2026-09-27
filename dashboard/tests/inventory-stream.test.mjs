import assert from 'node:assert/strict';
import test from 'node:test';
import { STALE_GRACE_MS, inventoryStatus, inventoryStream } from '../features/agents/inventory-stream.ts';

const flush = () => new Promise(resolve => setImmediate(resolve));

class FakeSocket extends EventTarget {
  readyState = 0;
  sent = [];
  closed = null;

  constructor(url) {
    super();
    this.url = url;
  }

  open() {
    this.readyState = 1;
    this.dispatchEvent(new Event('open'));
  }

  receive(value) {
    this.dispatchEvent(new MessageEvent('message', { data: JSON.stringify(value) }));
  }

  send(data) {
    this.sent.push(data);
  }

  close(code, reason) {
    if (this.readyState === 3) return;
    this.readyState = 3;
    this.closed = { code, reason };
    this.dispatchEvent(new Event('close'));
  }
}

function fakeTimers() {
  const pending = new Set();
  const delays = [];
  return {
    delays,
    pending,
    setTimeout(callback, ms) {
      const timer = { callback, ms };
      pending.add(timer);
      delays.push(ms);
      return timer;
    },
    clearTimeout(timer) {
      pending.delete(timer);
    },
    async fire() {
      assert.equal(pending.size, 1, 'one reconnect is scheduled');
      const [timer] = pending;
      pending.delete(timer);
      timer.callback();
      await flush();
    },
  };
}

const subscription = () => Response.json({
  websocket_url: 'wss://events.example/v1/agents/events',
  subscription_token: 'token',
  expires_at_unix_ms: Date.now() + 60_000,
});

const agent = (id, connected = true) => ({ id, name: id.toUpperCase(), connected });

function harness({ responses = [], online } = {}) {
  const sockets = [];
  const connections = [];
  const errors = [];
  const timers = fakeTimers();
  const revision = { current: -1 };
  const state = { agents: [], subscribeCalls: 0 };
  const stream = inventoryStream({
    subscribe: async () => {
      state.subscribeCalls++;
      const next = responses.length ? responses.shift() : subscription;
      return typeof next === 'function' ? next() : next;
    },
    openSocket: url => {
      const socket = new FakeSocket(url);
      sockets.push(socket);
      return socket;
    },
    renewal: () => ({ accept: value => value?.type === 'authorization', stop() {} }),
    revision,
    onAgents: update => { state.agents = update(state.agents); },
    onConnection: connection => connections.push(connection),
    onError: message => errors.push(message),
    online,
    timers,
  });
  return { stream, sockets, connections, errors, timers, revision, state };
}

const unavailable = () => new Response('', { status: 503 });

test('reconnects with backoff from 1 s doubling to 30 s', async () => {
  const { timers, errors, connections, state } = harness({ responses: Array(8).fill(unavailable) });
  await flush();
  for (let attempt = 0; attempt < 7; attempt++) await timers.fire();
  assert.deepEqual(timers.delays, [1_000, 2_000, 4_000, 8_000, 16_000, 30_000, 30_000, 30_000]);
  assert.equal(state.subscribeCalls, 8);
  assert.equal(errors.at(-1), 'The live Agent event stream could not be opened.');
  assert.deepEqual(connections, ['connecting', 'reconnecting']);
});

test('wake() while waiting connects now and resets the delay to 1 s', async () => {
  const { stream, timers, state } = harness({ responses: [unavailable, unavailable, unavailable, unavailable] });
  await flush();
  await timers.fire();
  assert.deepEqual(timers.delays, [1_000, 2_000]);
  stream.wake();
  await flush();
  assert.equal(state.subscribeCalls, 3);
  assert.deepEqual(timers.delays, [1_000, 2_000, 1_000]);
  assert.equal(timers.pending.size, 1);
});

test('wake() on an open stream asks for a fresh snapshot', async () => {
  const { stream, sockets, state } = harness();
  await flush();
  assert.equal(sockets.length, 1);
  assert.match(sockets[0].url, /token=token/);
  assert.match(sockets[0].url, /protocol=2/);
  stream.wake();
  assert.deepEqual(sockets[0].sent, [], 'a connecting socket cannot send yet');
  sockets[0].open();
  stream.wake();
  assert.deepEqual(sockets[0].sent, ['refresh']);
  assert.equal(state.subscribeCalls, 1);
});

test('does not subscribe twice while a request is in flight', async () => {
  let resolve;
  const { stream, state, sockets } = harness({ responses: [() => new Promise(done => { resolve = done; })] });
  await flush();
  stream.wake();
  stream.wake();
  assert.equal(state.subscribeCalls, 1);
  resolve(subscription());
  await flush();
  stream.wake();
  assert.equal(state.subscribeCalls, 1);
  assert.equal(sockets.length, 1);
});

test('applies deltas buffered before the snapshot, then live deltas in order', async () => {
  const { sockets, state, revision, connections, errors } = harness();
  await flush();
  const socket = sockets[0];
  socket.open();
  assert.deepEqual(errors, [null]);
  socket.receive({ type: 'authorization', connection_id: 'a'.repeat(64), expires_at_unix_ms: Date.now() + 60_000 });
  socket.receive({ type: 'agent_upsert', revision: 6, agent: agent('c') });
  socket.receive({ type: 'agent_deleted', revision: 5, agent_id: 'b' });
  assert.deepEqual(state.agents, []);
  assert.deepEqual(connections, ['connecting']);
  socket.receive({ type: 'snapshot', revision: 4, agents: [agent('b'), agent('a')], generated_at_unix_ms: 1 });
  assert.deepEqual(state.agents.map(item => item.id), ['a', 'c']);
  assert.equal(revision.current, 6);
  assert.deepEqual(connections, ['connecting', 'live']);

  socket.receive({ type: 'agent_upsert', revision: 7, agent: agent('a', false) });
  assert.deepEqual(state.agents.map(item => `${item.id}:${item.connected}`), ['c:true', 'a:false']);
  socket.receive({ type: 'agent_upsert', revision: 7, agent: agent('z') });
  assert.equal(state.agents.length, 2, 'an old revision is ignored');
  socket.receive({ type: 'agent_upsert', revision: 9, agent: agent('z') });
  assert.equal(state.agents.length, 2, 'a gap waits for a snapshot');
  assert.deepEqual(socket.sent, ['refresh']);
  socket.receive({ type: 'snapshot', revision: 8, agents: [agent('a')], generated_at_unix_ms: 2 });
  assert.deepEqual(state.agents.map(item => item.id), ['a', 'z']);
  assert.equal(revision.current, 9);
});

test('a snapshot older than the loaded inventory is refused and requested again', async () => {
  const { sockets, state, revision } = harness();
  revision.current = 10;
  await flush();
  sockets[0].open();
  sockets[0].receive({ type: 'snapshot', revision: 9, agents: [agent('a')], generated_at_unix_ms: 1 });
  assert.deepEqual(state.agents, []);
  assert.deepEqual(sockets[0].sent, ['refresh']);
});

test('a closed socket reports reconnecting, keeps the devices, and reconnects after 1 s', async () => {
  const { sockets, state, connections, timers } = harness();
  await flush();
  sockets[0].open();
  sockets[0].receive({ type: 'snapshot', revision: 1, agents: [agent('a')], generated_at_unix_ms: 1 });
  sockets[0].close(4001, 'fresh authorization required');
  assert.deepEqual(connections, ['connecting', 'live', 'reconnecting']);
  assert.deepEqual(state.agents.map(item => item.id), ['a']);
  assert.deepEqual(timers.delays, [1_000]);
  await timers.fire();
  assert.equal(sockets.length, 2);
  sockets[1].open();
  sockets[1].receive({ type: 'snapshot', revision: 2, agents: [agent('a'), agent('b')], generated_at_unix_ms: 2 });
  assert.deepEqual(connections, ['connecting', 'live', 'reconnecting', 'live']);
});

test('a refused subscription (403/404) stops retrying until wake()', async () => {
  for (const status of [403, 404]) {
    const { stream, timers, connections, errors, state, sockets } = harness({
      responses: [Response.json({ error: 'Company access denied' }, { status })],
    });
    await flush();
    assert.equal(timers.pending.size, 0, `${status} is not retried`);
    assert.deepEqual(connections, ['connecting', 'unavailable']);
    assert.deepEqual(errors, ['Company access denied']);
    stream.wake();
    await flush();
    assert.equal(state.subscribeCalls, 2);
    sockets[0].open();
    sockets[0].receive({ type: 'snapshot', revision: 1, agents: [], generated_at_unix_ms: 1 });
    assert.deepEqual(connections, ['connecting', 'unavailable', 'live']);
  }
});

test('a sign-in requirement stops the stream quietly', async () => {
  const { timers, errors, state } = harness({ responses: [null] });
  await flush();
  assert.equal(state.subscribeCalls, 1);
  assert.equal(timers.pending.size, 0);
  assert.deepEqual(errors, []);
});

test('offline overrides the stream state until the network returns', async () => {
  const { stream, sockets, connections } = harness({ online: false });
  await flush();
  assert.deepEqual(connections, ['offline']);
  sockets[0].open();
  sockets[0].receive({ type: 'snapshot', revision: 1, agents: [], generated_at_unix_ms: 1 });
  assert.deepEqual(connections, ['offline']);
  stream.setOnline(true);
  assert.deepEqual(connections, ['offline', 'live']);
  stream.setOnline(false);
  assert.deepEqual(connections, ['offline', 'live', 'offline']);
});

test('stop() closes the socket and cancels reconnects', async () => {
  const first = harness();
  await flush();
  first.stream.stop();
  assert.deepEqual(first.sockets[0].closed, { code: 1000, reason: 'dashboard subscription ended' });
  assert.equal(first.timers.pending.size, 0);

  let resolve;
  const second = harness({ responses: [() => new Promise(done => { resolve = done; })] });
  await flush();
  second.stream.stop();
  resolve(subscription());
  await flush();
  assert.equal(second.sockets.length, 0, 'a late subscription opens nothing');
});

test('inventoryStatus waits out the grace period before calling data stale', () => {
  const since = 1_000_000;
  const cases = [
    [{ hasData: false, connection: 'live', since, now: since }, 'loading'],
    [{ hasData: false, connection: 'reconnecting', since, now: since + 60_000 }, 'loading'],
    [{ hasData: true, connection: 'live', since: null, now: 0 }, 'live'],
    [{ hasData: true, connection: 'reconnecting', since, now: since }, 'live'],
    [{ hasData: true, connection: 'reconnecting', since, now: since + STALE_GRACE_MS - 1 }, 'live'],
    [{ hasData: true, connection: 'reconnecting', since, now: since + STALE_GRACE_MS }, 'stale'],
    [{ hasData: true, connection: 'offline', since, now: since + 1_000 }, 'live'],
    [{ hasData: true, connection: 'offline', since, now: since + STALE_GRACE_MS }, 'stale'],
    [{ hasData: true, connection: 'unavailable', since, now: since + STALE_GRACE_MS }, 'stale'],
    [{ hasData: true, connection: 'connecting', since: null, now: 0 }, 'stale'],
    // The hook's clock lags `since` until the grace timer fires.
    [{ hasData: true, connection: 'reconnecting', since, now: since - 90_000 }, 'live'],
  ];
  for (const [input, expected] of cases) assert.equal(inventoryStatus(input), expected, JSON.stringify(input));
  assert.equal(STALE_GRACE_MS, 5_000);
});
