import assert from 'node:assert/strict';
import test from 'node:test';
import { ACCESS_REVOKED_CLOSE_CODE, STALE_GRACE_MS, eventsSocketUrl, inventoryStatus, inventoryStream } from '../features/agents/inventory-stream.ts';

class FakeSocket extends EventTarget {
  readyState = 0;
  sent = [];
  closed = null;

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
    this.dispatchEvent(Object.assign(new Event('close'), { code }));
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
    fire() {
      assert.equal(pending.size, 1, 'one reconnect is scheduled');
      const [timer] = pending;
      pending.delete(timer);
      timer.callback();
    },
  };
}

const agent = (id, connected = true) => ({ id, name: id.toUpperCase(), connected });

function harness({ online } = {}) {
  const sockets = [];
  const connections = [];
  const timers = fakeTimers();
  const revision = { current: -1 };
  const state = { agents: [], refused: 0, readings: [] };
  const stream = inventoryStream({
    openSocket: () => {
      const socket = new FakeSocket();
      sockets.push(socket);
      return socket;
    },
    revision,
    onAgents: update => { state.agents = update(state.agents); },
    onMetrics: readings => { state.readings.push(...readings); },
    onConnection: connection => connections.push(connection),
    onRefused: () => { state.refused++; },
    online,
    timers,
  });
  return { stream, sockets, connections, timers, revision, state };
}

// A socket the server never accepted closes without opening.
const refuse = socket => socket.close(1006);

test('the socket is on the page’s own origin', () => {
  assert.equal(eventsSocketUrl({ protocol: 'https:', host: 'rmm.example.com' }), 'wss://rmm.example.com/v1/events');
  assert.equal(eventsSocketUrl({ protocol: 'http:', host: 'localhost:5173' }), 'ws://localhost:5173/v1/events');
});

test('reconnects with backoff from 1 s doubling to 30 s', () => {
  const { sockets, timers, connections } = harness();
  for (let attempt = 0; attempt < 8; attempt++) {
    refuse(sockets.at(-1));
    if (attempt < 7) timers.fire();
  }
  assert.deepEqual(timers.delays, [1_000, 2_000, 4_000, 8_000, 16_000, 30_000, 30_000, 30_000]);
  assert.equal(sockets.length, 8);
  assert.deepEqual(connections, ['connecting', 'reconnecting']);
});

test('a socket that never opened asks the caller to check the session', () => {
  const { sockets, state } = harness();
  refuse(sockets[0]);
  assert.equal(state.refused, 1);
});

test('losing access closes with 4001, which also asks for a session check', () => {
  const { sockets, state, timers } = harness();
  sockets[0].open();
  sockets[0].close(1011, 'try again');
  assert.equal(state.refused, 0, 'a server error after opening is not about access');
  timers.fire();
  sockets[1].open();
  sockets[1].close(ACCESS_REVOKED_CLOSE_CODE, 'sign in again');
  assert.equal(state.refused, 1);
});

test('wake() while waiting connects now and resets the delay to 1 s', () => {
  const { stream, sockets, timers } = harness();
  refuse(sockets[0]);
  timers.fire();
  refuse(sockets[1]);
  assert.deepEqual(timers.delays, [1_000, 2_000]);
  stream.wake();
  assert.equal(sockets.length, 3);
  assert.equal(timers.pending.size, 0);
  refuse(sockets[2]);
  assert.deepEqual(timers.delays, [1_000, 2_000, 1_000]);
});

test('wake() on an open socket asks for a fresh snapshot', () => {
  const { stream, sockets } = harness();
  stream.wake();
  assert.deepEqual(sockets[0].sent, [], 'a connecting socket cannot send yet');
  assert.equal(sockets.length, 1, 'a connecting socket is not replaced');
  sockets[0].open();
  stream.wake();
  assert.deepEqual(sockets[0].sent, ['refresh']);
});

test('applies deltas buffered before the snapshot, then live deltas in order', () => {
  const { sockets, state, revision, connections } = harness();
  const socket = sockets[0];
  socket.open();
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

test('a snapshot older than the loaded inventory is refused and requested again', () => {
  const { sockets, state, revision } = harness();
  revision.current = 10;
  sockets[0].open();
  sockets[0].receive({ type: 'snapshot', revision: 9, agents: [agent('a')], generated_at_unix_ms: 1 });
  assert.deepEqual(state.agents, []);
  assert.deepEqual(sockets[0].sent, ['refresh']);
});

test('an unreadable message asks for a snapshot', () => {
  const { sockets } = harness();
  sockets[0].open();
  sockets[0].receive({ type: 'mystery', revision: 1 });
  sockets[0].dispatchEvent(new MessageEvent('message', { data: '{' }));
  assert.deepEqual(sockets[0].sent, ['refresh', 'refresh']);
});

test('a closed socket reports reconnecting, keeps the devices, and reconnects after 1 s', () => {
  const { sockets, state, connections, timers } = harness();
  sockets[0].open();
  sockets[0].receive({ type: 'snapshot', revision: 1, agents: [agent('a')], generated_at_unix_ms: 1 });
  sockets[0].close(1001, 'going away');
  assert.deepEqual(connections, ['connecting', 'live', 'reconnecting']);
  assert.deepEqual(state.agents.map(item => item.id), ['a']);
  assert.deepEqual(timers.delays, [1_000]);
  timers.fire();
  assert.equal(sockets.length, 2);
  sockets[1].open();
  sockets[1].receive({ type: 'snapshot', revision: 2, agents: [agent('a'), agent('b')], generated_at_unix_ms: 2 });
  assert.deepEqual(connections, ['connecting', 'live', 'reconnecting', 'live']);
});

test('offline overrides the stream state until the network returns', () => {
  const { stream, sockets, connections } = harness({ online: false });
  assert.deepEqual(connections, ['offline']);
  sockets[0].open();
  sockets[0].receive({ type: 'snapshot', revision: 1, agents: [], generated_at_unix_ms: 1 });
  assert.deepEqual(connections, ['offline']);
  stream.setOnline(true);
  assert.deepEqual(connections, ['offline', 'live']);
  stream.setOnline(false);
  assert.deepEqual(connections, ['offline', 'live', 'offline']);
});

test('stop() closes the socket and cancels reconnects', () => {
  const first = harness();
  first.stream.stop();
  assert.deepEqual(first.sockets[0].closed, { code: 1000, reason: 'website closed the inventory' });
  assert.equal(first.timers.pending.size, 0);

  const second = harness();
  refuse(second.sockets[0]);
  assert.equal(second.timers.pending.size, 1);
  second.stream.stop();
  assert.equal(second.timers.pending.size, 0, 'a pending reconnect is cancelled');
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
    [{ hasData: true, connection: 'connecting', since: null, now: 0 }, 'stale'],
    // The hook's clock lags `since` until the grace timer fires.
    [{ hasData: true, connection: 'reconnecting', since, now: since - 90_000 }, 'live'],
  ];
  for (const [input, expected] of cases) assert.equal(inventoryStatus(input), expected, JSON.stringify(input));
  assert.equal(STALE_GRACE_MS, 5_000);
});

test('resource usage arrives beside the revisions without disturbing them', () => {
  const { sockets, state, revision } = harness();
  sockets[0].open();
  sockets[0].receive({ type: 'snapshot', revision: 3, agents: [agent('a')], generated_at_unix_ms: 0 });
  const reading = {
    device_id: 'a', at: 10, cpu_percent: 12.5, memory_used_bytes: 1, memory_total_bytes: 2,
    network_received_bytes_per_second: 3, network_sent_bytes_per_second: 4, uptime_seconds: 5,
    volumes: [{ name: 'C:', total_bytes: 10, free_bytes: 4 }],
  };
  sockets[0].receive({ type: 'metrics', readings: [reading, { device_id: 'b' }] });
  assert.deepEqual(state.readings, [reading], 'invalid readings are skipped');
  assert.equal(revision.current, 3);
  assert.deepEqual(sockets[0].sent, [], 'no snapshot is requested');
});
