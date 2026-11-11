import assert from 'node:assert/strict';
import test from 'node:test';
import { DeviceStore } from '../features/agents/device-store.ts';
import { ACCESS_REVOKED_CLOSE_CODE, PING, PROBE_ANSWER_MS, REFRESH, SILENT_SOCKET_MS, SNAPSHOT_ANSWER_MS, STALE_GRACE_MS, eventsSocketUrl, inventoryStatus, inventoryStream, refreshInventory, watchMessage } from '../features/agents/inventory-stream.ts';

class FakeSocket extends EventTarget {
  readyState = 0;
  sent = [];
  closed = null;

  open() {
    this.readyState = 1;
    this.dispatchEvent(new Event('open'));
  }

  receive(value) {
    this.receiveRaw(JSON.stringify(value));
  }

  receiveRaw(data) {
    this.dispatchEvent(new MessageEvent('message', { data }));
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

// Firing a timer moves the clock on by its delay.
function fakeTimers() {
  const pending = new Set();
  const delays = [];
  let clock = 0;
  return {
    delays,
    pending,
    now: () => clock,
    setTimeout(callback, ms) {
      const timer = { callback, ms };
      pending.add(timer);
      delays.push(ms);
      return timer;
    },
    clearTimeout(timer) {
      pending.delete(timer);
    },
    advance(ms) {
      clock += ms;
    },
    fire() {
      assert.equal(pending.size, 1, 'one reconnect is scheduled');
      const [timer] = pending;
      pending.delete(timer);
      clock += timer.ms;
      timer.callback();
    },
  };
}

const agent = (id, connected = true) => ({ id, name: id.toUpperCase(), platform: "windows", connected });

// `random` 0.5 makes each reconnect wait three quarters of its step. Setting
// `state.failing` makes every listener throw, as a page's bug would.
function harness({ online, random = () => 0.5, watch } = {}) {
  const sockets = [];
  const connections = [];
  const timers = fakeTimers();
  const revision = { current: -1 };
  const devices = new DeviceStore({ schedule: () => {} });
  const state = {
    get agents() { return devices.agents(); },
    get companies() { return devices.companies(); },
    refused: 0, refusedAt: [], readings: [], alerts: [], jobs: [], updates: [], errors: [], failing: false,
  };
  const fail = name => {
    if (state.failing) throw new Error(`${name} listener failed`);
  };
  const stream = inventoryStream({
    openSocket: () => {
      const socket = new FakeSocket();
      sockets.push(socket);
      return socket;
    },
    revision,
    inventory: {
      replace: inventory => { fail('inventory'); devices.replace(inventory); },
      apply: change => { fail('inventory'); devices.apply(change); },
    },
    onMetrics: readings => { fail('metrics'); state.readings.push(...readings); },
    watch,
    onAlerts: event => { fail('alerts'); state.alerts.push(event); },
    onJobs: event => { fail('jobs'); state.jobs.push(event); },
    onUpdates: event => { fail('updates'); state.updates.push(event); },
    onConnection: connection => connections.push(connection),
    onRefused: () => {
      state.refused++;
      state.refusedAt.push(timers.now());
    },
    online,
    timers,
    random,
    reportError: error => { state.errors.push(error.message); },
  });
  return { stream, sockets, connections, timers, revision, state, devices };
}

// A socket the server never accepted closes without opening.
const refuse = socket => socket.close(1006);

test('the socket is on the page’s own origin', () => {
  assert.equal(eventsSocketUrl({ protocol: 'https:', host: 'rmm.example.com' }), 'wss://rmm.example.com/v1/events');
  assert.equal(eventsSocketUrl({ protocol: 'http:', host: 'localhost:5173' }), 'ws://localhost:5173/v1/events');
});

// The waits before the next `attempts` reconnects, each socket refused.
function refusedDelays(random, attempts = 8) {
  const { sockets, timers, connections } = harness({ random });
  for (let attempt = 0; attempt < attempts; attempt++) {
    refuse(sockets.at(-1));
    if (attempt < attempts - 1) timers.fire();
  }
  assert.equal(sockets.length, attempts);
  assert.deepEqual(connections, ['connecting', 'reconnecting']);
  return timers.delays;
}

test('each reconnect waits between half its step and the step, which doubles from 1 s to 30 s', () => {
  const steps = [1_000, 2_000, 4_000, 8_000, 16_000, 30_000, 30_000, 30_000];
  assert.deepEqual(refusedDelays(() => 0), steps.map(step => step / 2));
  assert.deepEqual(refusedDelays(() => 0.5), steps.map(step => step * 0.75));
  const longest = refusedDelays(() => 0.999_999, 20);
  for (const [index, delay] of longest.entries()) {
    assert.ok(delay <= Math.min(1_000 * 2 ** index, 30_000), `wait ${index} is ${delay} ms`);
    assert.ok(delay > Math.min(1_000 * 2 ** index, 30_000) * 0.99, `wait ${index} is ${delay} ms`);
  }
});

test('a socket that opens and fails before its snapshot keeps the backoff growing', () => {
  const { sockets, timers, connections } = harness();
  for (let attempt = 0; attempt < 3; attempt++) {
    sockets.at(-1).open();
    // The server closes with 1011 when it cannot read the devices.
    sockets.at(-1).close(1011, 'devices unavailable');
    timers.fire();
  }
  assert.deepEqual(timers.delays, [750, 1_500, 3_000]);
  assert.deepEqual(connections, ['connecting', 'reconnecting']);
});

test('a socket that delivers its snapshot resets the backoff', () => {
  const { sockets, timers } = harness();
  refuse(sockets[0]);
  timers.fire();
  refuse(sockets[1]);
  timers.fire();
  sockets[2].open();
  sockets[2].receive({ type: 'snapshot', revision: 1, agents: [], generated_at_unix_ms: 1 });
  sockets[2].close(1001, 'going away');
  assert.deepEqual(timers.delays, [750, 1_500, 750]);
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

test('while the server stays unreachable, refused sockets check the session about once a minute', () => {
  const { sockets, state, timers } = harness();
  while (timers.now() < 10 * 60_000) {
    refuse(sockets.at(-1));
    timers.fire();
  }
  assert.equal(state.refusedAt[0], 0, 'the first refusal checks at once');
  const gaps = state.refusedAt.slice(1).map((at, index) => at - state.refusedAt[index]);
  assert.ok(gaps.length >= 7, `${state.refusedAt.length} checks in ten minutes`);
  for (const gap of gaps) assert.ok(gap >= 60_000 && gap < 60_000 + 30_000, `checks ${gap} ms apart`);
});

test('4001 checks the session at once, even just after a refused socket did', () => {
  const { sockets, state, timers } = harness();
  refuse(sockets[0]);
  timers.fire();
  sockets[1].open();
  sockets[1].close(ACCESS_REVOKED_CLOSE_CODE, 'sign in again');
  assert.deepEqual(state.refusedAt, [0, 750]);
  timers.fire();
  refuse(sockets[2]);
  assert.deepEqual(state.refusedAt, [0, 750], 'a refusal straight after is covered by that check');
});

test('after the stream goes live, the next outage’s first refusal checks at once', () => {
  const { sockets, state, timers } = harness();
  refuse(sockets[0]);
  timers.fire();
  sockets[1].open();
  sockets[1].receive({ type: 'snapshot', revision: 1, agents: [], generated_at_unix_ms: 1 });
  sockets[1].close(1001, 'going away');
  timers.fire();
  refuse(sockets[2]);
  assert.deepEqual(state.refusedAt, [0, 1_500]);
});

test('wake() while waiting connects at once and keeps the backoff step', () => {
  const { stream, sockets, timers } = harness();
  refuse(sockets[0]);
  timers.fire();
  refuse(sockets[1]);
  assert.deepEqual(timers.delays, [750, 1_500]);
  stream.wake();
  assert.equal(sockets.length, 3);
  assert.equal(timers.pending.size, 0, 'the wait is skipped');
  refuse(sockets[2]);
  assert.deepEqual(timers.delays, [750, 1_500, 3_000], 'only going live restarts the step');
});

test('wake() on a live socket asks for nothing', () => {
  const { stream, sockets, timers, connections } = harness();
  sockets[0].open();
  sockets[0].receive({ type: 'snapshot', revision: 1, agents: [agent('a')], generated_at_unix_ms: 1 });
  stream.wake();
  timers.advance(20_000);
  sockets[0].receive({ type: 'metrics', readings: [] });
  timers.advance(SILENT_SOCKET_MS);
  stream.wake();
  assert.deepEqual(sockets[0].sent, [], 'the socket kept the page current in the background');
  assert.equal(sockets[0].closed, null);
  assert.equal(sockets.length, 1);
  assert.equal(timers.pending.size, 0);
  assert.deepEqual(connections, ['connecting', 'live']);
});

test('wake() asks a live socket that has been silent too long for a pong, and keeps it if one comes', () => {
  const { stream, sockets, timers, connections } = harness();
  sockets[0].open();
  sockets[0].receive({ type: 'snapshot', revision: 50, agents: [agent('a')], generated_at_unix_ms: 1 });
  // A tab that watches no device's readings may hear nothing for long.
  timers.advance(SILENT_SOCKET_MS + 1);
  stream.wake();
  stream.wake();
  assert.deepEqual(sockets[0].sent, [PING], 'asked once, not a snapshot');
  sockets[0].receive({ type: 'pong' });
  assert.equal(timers.pending.size, 0, 'the answer ends the wait');
  assert.equal(sockets[0].closed, null);
  assert.equal(sockets.length, 1);
  assert.deepEqual(connections, ['connecting', 'live']);
});

test('wake() replaces a silent live socket that does not answer its ping in time', () => {
  const { stream, sockets, timers, connections, revision, state } = harness();
  sockets[0].open();
  sockets[0].receive({ type: 'snapshot', revision: 50, agents: [agent('a')], generated_at_unix_ms: 1 });
  timers.advance(SILENT_SOCKET_MS + 1);
  stream.wake();
  assert.deepEqual(sockets[0].sent, [PING], 'a dead socket would deliver no snapshot');
  assert.equal(sockets[0].closed, null, 'until the wait is up');
  timers.fire();
  assert.deepEqual(timers.delays, [PROBE_ANSWER_MS]);
  assert.equal(sockets[0].closed?.code, 1000);
  assert.equal(sockets.length, 2, 'a new socket opens at once');
  assert.equal(timers.pending.size, 0);
  assert.equal(revision.current, -1);
  assert.deepEqual(connections, ['connecting', 'live', 'reconnecting']);
  sockets[1].open();
  sockets[1].receive({ type: 'snapshot', revision: 3, agents: [agent('a'), agent('b')], generated_at_unix_ms: 2 });
  assert.deepEqual(state.agents.map(item => item.id), ['a', 'b']);
  assert.deepEqual(connections, ['connecting', 'live', 'reconnecting', 'live']);
});

test('wake() before the first snapshot asks for nothing', () => {
  const { stream, sockets, timers } = harness();
  stream.wake();
  sockets[0].open();
  stream.wake();
  timers.advance(SILENT_SOCKET_MS + 1);
  stream.wake();
  assert.deepEqual(sockets[0].sent, [], 'the snapshot is already on its way');
  assert.equal(sockets[0].closed, null);
  assert.equal(sockets.length, 1);
});

// The Refresh button over a harness's stream, counting the list loads it
// starts, as the page's /v1/agents requests.
function refresher({ stream, connections }) {
  const counts = { loads: 0 };
  counts.press = () => refreshInventory(connections.at(-1), {
    wake: () => stream.wake(),
    load: async () => { counts.loads++; return true; },
  });
  return counts;
}

test('Refresh on a live stream downloads no list and asks the server for nothing', async () => {
  const h = harness();
  const refresh = refresher(h);
  h.sockets[0].open();
  h.sockets[0].receive({ type: 'snapshot', revision: 1, agents: [agent('a')], generated_at_unix_ms: 1 });
  assert.equal(h.connections.at(-1), 'live');
  assert.equal(await refresh.press(), true);
  assert.equal(refresh.loads, 0, 'the stream already holds every change');
  assert.deepEqual(h.sockets[0].sent, [], 'no snapshot is asked for either');
  assert.equal(h.sockets.length, 1);
  assert.equal(h.sockets[0].closed, null);
});

test('Refresh on a live stream that went silent replaces its socket if it is dead, still without a list download', async () => {
  const h = harness();
  const refresh = refresher(h);
  h.sockets[0].open();
  h.sockets[0].receive({ type: 'snapshot', revision: 1, agents: [agent('a')], generated_at_unix_ms: 1 });
  h.timers.advance(SILENT_SOCKET_MS + 1);
  await refresh.press();
  h.timers.fire();
  assert.equal(refresh.loads, 0, "the new socket's snapshot brings the devices");
  assert.equal(h.sockets[0].closed?.code, 1000);
  assert.equal(h.sockets.length, 2);
});

test('Refresh while the stream is down reconnects at once and loads the list', async () => {
  const h = harness();
  const refresh = refresher(h);
  h.sockets[0].open();
  h.sockets[0].receive({ type: 'snapshot', revision: 1, agents: [agent('a')], generated_at_unix_ms: 1 });
  h.sockets[0].close(1006);
  assert.equal(h.connections.at(-1), 'reconnecting');
  await refresh.press();
  assert.equal(refresh.loads, 1);
  assert.equal(h.sockets.length, 2, 'without waiting out the backoff');
  assert.equal(h.timers.pending.size, 0);
});

test('Refresh before the first snapshot and while offline loads the list', async () => {
  const connecting = harness();
  const first = refresher(connecting);
  await first.press();
  assert.equal(connecting.connections.at(-1), 'connecting');
  assert.equal(first.loads, 1);
  assert.equal(connecting.sockets.length, 1, 'the socket still connecting is kept');
  const offline = harness({ online: false });
  const second = refresher(offline);
  await second.press();
  assert.equal(offline.connections.at(-1), 'offline');
  assert.equal(second.loads, 1);
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
  assert.deepEqual(socket.sent, [REFRESH]);
  socket.receive({ type: 'snapshot', revision: 8, agents: [agent('a')], generated_at_unix_ms: 2 });
  assert.deepEqual(state.agents.map(item => item.id), ['a', 'z']);
  assert.equal(revision.current, 9);
});

test('companies arrive in the snapshot and change in order with devices', () => {
  const { sockets, state } = harness();
  const socket = sockets[0];
  socket.open();
  socket.receive({ type: 'company_upsert', revision: 3, company: { id: 'n', name: 'New' } });
  socket.receive({
    type: 'snapshot',
    revision: 2,
    agents: [{ ...agent('a'), company_id: 'g' }],
    companies: [{ id: 'g', name: 'Globex' }, { id: 'c', name: 'Acme' }],
    generated_at_unix_ms: 1,
  });
  assert.deepEqual(state.companies.map(company => company.name), ['Acme', 'Globex', 'New']);
  socket.receive({ type: 'agent_upsert', revision: 4, agent: { ...agent('a'), company_id: null } });
  socket.receive({ type: 'company_deleted', revision: 5, company_id: 'g' });
  assert.deepEqual(state.companies.map(company => company.id), ['c', 'n']);
  assert.equal(state.agents[0].company_id, null);
});

test('after a server restart, the new socket’s lower-revision snapshot applies without a refresh', () => {
  const { sockets, state, revision, connections, timers } = harness();
  sockets[0].open();
  sockets[0].receive({ type: 'snapshot', revision: 80_000, agents: [agent('a')], generated_at_unix_ms: 1 });
  sockets[0].close(1012, 'service restart');
  assert.equal(revision.current, -1, 'an HTTP load while the socket is down applies');
  timers.fire();
  sockets[1].open();
  sockets[1].receive({ type: 'snapshot', revision: 100, agents: [agent('a'), agent('b')], generated_at_unix_ms: 2 });
  assert.deepEqual(state.agents.map(item => item.id), ['a', 'b']);
  assert.equal(revision.current, 100);
  assert.deepEqual(sockets[1].sent, []);
  assert.deepEqual(connections, ['connecting', 'live', 'reconnecting', 'live']);
  sockets[1].receive({ type: 'agent_deleted', revision: 101, agent_id: 'a' });
  assert.deepEqual(state.agents.map(item => item.id), ['b']);
});

test('a socket’s first snapshot applies whatever revision the page last saw', () => {
  const { sockets, state, revision } = harness();
  revision.current = 10;
  sockets[0].open();
  sockets[0].receive({ type: 'snapshot', revision: 9, agents: [agent('a')], generated_at_unix_ms: 1 });
  assert.deepEqual(state.agents.map(item => item.id), ['a']);
  assert.equal(revision.current, 9);
  assert.deepEqual(sockets[0].sent, []);
});

test('a later snapshot overtaken by an HTTP load keeps the newer inventory without a refresh', () => {
  const { sockets, state, revision, connections, devices } = harness();
  const socket = sockets[0];
  socket.open();
  socket.receive({ type: 'snapshot', revision: 5, agents: [agent('a')], generated_at_unix_ms: 1 });
  socket.receive({ type: 'agent_upsert', revision: 7, agent: agent('c') });
  assert.deepEqual(socket.sent, [REFRESH], 'the gap asks for a snapshot');
  // The HTTP load answers first, with revision 9.
  revision.current = 9;
  devices.replace({ agents: [agent('a'), agent('b'), agent('c')], companies: [], tags: [] });
  socket.receive({ type: 'agent_upsert', revision: 8, agent: agent('z') });
  socket.receive({ type: 'snapshot', revision: 8, agents: [agent('a'), agent('c'), agent('z')], generated_at_unix_ms: 2 });
  assert.deepEqual(state.agents.map(item => item.id), ['a', 'b', 'c']);
  assert.equal(revision.current, 9);
  assert.deepEqual(socket.sent, [REFRESH], 'the older snapshot is not asked for again');
  assert.deepEqual(connections, ['connecting', 'live']);
  socket.receive({ type: 'agent_upsert', revision: 9, agent: agent('y') });
  socket.receive({ type: 'agent_deleted', revision: 10, agent_id: 'b' });
  assert.deepEqual(state.agents.map(item => item.id), ['a', 'c']);
  assert.equal(revision.current, 10);
});

test('a message of a type the page doesn’t know is ignored', () => {
  const { sockets, state, revision, connections } = harness();
  const socket = sockets[0];
  socket.open();
  socket.receive({ type: 'snapshot', revision: 3, agents: [agent('a')], generated_at_unix_ms: 1 });
  socket.receive({ type: 'mystery', device_id: 'a' });
  socket.receive({ type: 'agent_upsert', revision: 4, agent: agent('b') });
  assert.deepEqual(state.agents.map(item => item.id), ['a', 'b']);
  // A change a newer server added takes its revision, so the next isn't a gap.
  socket.receive({ type: 'site_upsert', revision: 5, site: { id: 's' } });
  socket.receive({ type: 'agent_deleted', revision: 6, agent_id: 'a' });
  assert.deepEqual(state.agents.map(item => item.id), ['b']);
  assert.equal(revision.current, 6);
  assert.deepEqual(socket.sent, []);
  assert.equal(socket.closed, null);
  assert.deepEqual(connections, ['connecting', 'live']);
});

test('an unknown change before the snapshot waits with the others', () => {
  const { sockets, state, revision } = harness();
  const socket = sockets[0];
  socket.open();
  socket.receive({ type: 'agent_upsert', revision: 6, agent: agent('c') });
  socket.receive({ type: 'site_upsert', revision: 5, site: { id: 's' } });
  socket.receive({ type: 'snapshot', revision: 4, agents: [agent('a')], generated_at_unix_ms: 1 });
  assert.deepEqual(state.agents.map(item => item.id), ['a', 'c']);
  assert.equal(revision.current, 6);
  assert.deepEqual(socket.sent, []);
});

test('unreadable messages that carry no revision are dropped', () => {
  const { sockets, state, revision } = harness();
  const socket = sockets[0];
  socket.open();
  socket.receive({ type: 'snapshot', revision: 3, agents: [agent('a')], generated_at_unix_ms: 1 });
  for (const data of ['{', 'refresh', '', 'null', '[]', '7', '{"type":5}', '{"revision":4}']) socket.receiveRaw(data);
  socket.receive({ type: 'metrics', readings: 'none' });
  socket.receive({ type: 'alert_upsert', alert: { id: 'x' } });
  socket.receive({ type: 'alert_counts' });
  socket.receive({ type: 'monitor_update', device_id: 'a' });
  socket.receive({ type: 'job_update', job: null });
  socket.receive({ type: 'device_updates', device: 'a' });
  socket.receive({ type: 'update_run' });
  assert.deepEqual(socket.sent, []);
  assert.deepEqual([state.readings, state.alerts, state.jobs, state.updates], [[], [], [], []]);
  socket.receive({ type: 'agent_upsert', revision: 4, agent: agent('b') });
  assert.deepEqual(state.agents.map(item => item.id), ['a', 'b']);
  assert.equal(revision.current, 4);
  assert.deepEqual(state.errors, []);
});

test('an unreadable device change asks for one snapshot and holds later changes for it', () => {
  const { sockets, state, revision, connections } = harness();
  const socket = sockets[0];
  socket.open();
  socket.receive({ type: 'snapshot', revision: 3, agents: [agent('a')], generated_at_unix_ms: 1 });
  socket.receive({ type: 'agent_upsert', revision: 4, agent: { id: 'b' } });
  assert.deepEqual(socket.sent, [REFRESH]);
  socket.receive({ type: 'agent_upsert', revision: 5, agent: agent('c') });
  socket.receive({ type: 'agent_deleted', revision: 6 });
  socket.receive({ type: 'company_upsert', revision: 7, company: { name: 'No ID' } });
  assert.deepEqual(state.agents.map(item => item.id), ['a'], 'the changes after it wait');
  assert.deepEqual(socket.sent, [REFRESH], 'the snapshot asked for covers them all');
  socket.receive({ type: 'snapshot', revision: 7, agents: [agent('a'), agent('b'), agent('c')], generated_at_unix_ms: 2 });
  socket.receive({ type: 'agent_upsert', revision: 8, agent: agent('e') });
  assert.deepEqual(state.agents.map(item => item.id), ['a', 'b', 'c', 'e']);
  assert.equal(revision.current, 8);
  assert.deepEqual(socket.sent, [REFRESH]);
  assert.deepEqual(connections, ['connecting', 'live']);
});

test('a burst of gaps and unreadable changes sends one refresh until its snapshot arrives', () => {
  const { sockets, state, revision } = harness();
  const socket = sockets[0];
  socket.open();
  socket.receive({ type: 'snapshot', revision: 1, agents: [agent('a')], generated_at_unix_ms: 1 });
  for (const next of [3, 5, 4, 9, 7]) {
    socket.receive({ type: 'agent_upsert', revision: next, agent: agent(`d${next}`) });
    socket.receive({ type: 'tag_upsert', revision: next + 10, tag: null });
  }
  assert.deepEqual(socket.sent, [REFRESH]);
  socket.receive({ type: 'snapshot', revision: 20, agents: [agent('a')], generated_at_unix_ms: 2 });
  assert.equal(revision.current, 20);
  // Once answered, the next gap may ask again.
  socket.receive({ type: 'agent_upsert', revision: 22, agent: agent('z') });
  socket.receive({ type: 'agent_upsert', revision: 23, agent: agent('y') });
  assert.deepEqual(socket.sent, [REFRESH, REFRESH]);
  assert.deepEqual(state.agents.map(item => item.id), ['a']);
});

test('a refresh unanswered for too long replaces the socket', () => {
  const { sockets, timers, connections, revision } = harness();
  sockets[0].open();
  sockets[0].receive({ type: 'snapshot', revision: 1, agents: [agent('a')], generated_at_unix_ms: 1 });
  sockets[0].receive({ type: 'agent_upsert', revision: 3, agent: agent('b') });
  timers.advance(SNAPSHOT_ANSWER_MS);
  sockets[0].receive({ type: 'metrics', readings: [] });
  assert.equal(sockets[0].closed, null, 'the snapshot may still be on its way');
  timers.advance(1);
  sockets[0].receive({ type: 'metrics', readings: [] });
  assert.deepEqual(sockets[0].sent, [REFRESH], 'asking again on the same socket would go unread too');
  assert.equal(sockets[0].closed?.code, 1000);
  assert.equal(revision.current, -1);
  assert.deepEqual(connections, ['connecting', 'live', 'reconnecting']);
  timers.fire();
  sockets[1].open();
  sockets[1].receive({ type: 'snapshot', revision: 3, agents: [agent('a'), agent('b')], generated_at_unix_ms: 2 });
  assert.deepEqual(connections, ['connecting', 'live', 'reconnecting', 'live']);
});

test('an unreadable snapshot replaces the socket after the growing reconnect wait, never asking again', () => {
  const { sockets, timers, connections } = harness();
  for (let attempt = 0; attempt < 3; attempt++) {
    sockets[attempt].open();
    sockets[attempt].receive({ type: 'snapshot', revision: 1, agents: [{ id: 'a' }], generated_at_unix_ms: 1 });
    assert.deepEqual(sockets[attempt].sent, []);
    assert.equal(sockets[attempt].closed?.code, 1000);
    timers.fire();
  }
  assert.deepEqual(timers.delays, [750, 1_500, 3_000]);
  assert.deepEqual(connections, ['connecting', 'reconnecting']);
});

test('a page listener that throws is reported, asks for nothing, and later events still arrive', () => {
  const { sockets, state, revision, connections } = harness();
  const socket = sockets[0];
  socket.open();
  socket.receive({ type: 'snapshot', revision: 3, agents: [agent('a')], generated_at_unix_ms: 1 });
  const counts = { open: 0, acknowledged: 0, open_info: 0, open_warning: 0, open_critical: 0 };
  state.failing = true;
  socket.receive({ type: 'metrics', readings: [] });
  socket.receive({ type: 'alert_counts', counts });
  socket.receive({ type: 'jobs_resync' });
  socket.receive({ type: 'updates_resync' });
  assert.deepEqual(state.errors, ['metrics listener failed', 'alerts listener failed', 'jobs listener failed', 'updates listener failed']);
  assert.deepEqual(socket.sent, []);
  assert.equal(socket.closed, null);
  state.failing = false;
  socket.receive({ type: 'alert_counts', counts });
  socket.receive({ type: 'jobs_resync' });
  socket.receive({ type: 'updates_resync' });
  socket.receive({ type: 'agent_upsert', revision: 4, agent: agent('b') });
  assert.deepEqual([state.alerts.length, state.jobs.length, state.updates.length], [1, 1, 1]);
  assert.deepEqual(state.agents.map(item => item.id), ['a', 'b']);
  assert.equal(revision.current, 4);
  assert.deepEqual(connections, ['connecting', 'live']);
});

test('a device change that fails to apply is reported and replaces the socket instead of asking for a refresh', () => {
  const { sockets, state, timers, connections, revision } = harness();
  sockets[0].open();
  sockets[0].receive({ type: 'snapshot', revision: 3, agents: [agent('a')], generated_at_unix_ms: 1 });
  state.failing = true;
  sockets[0].receive({ type: 'agent_upsert', revision: 4, agent: agent('b') });
  assert.deepEqual(state.errors, ['inventory listener failed']);
  assert.deepEqual(sockets[0].sent, []);
  assert.equal(sockets[0].closed?.code, 1000);
  assert.equal(revision.current, -1, 'the next socket’s snapshot rebuilds the inventory');
  assert.deepEqual(connections, ['connecting', 'live', 'reconnecting']);
  assert.deepEqual(timers.delays, [750]);
  state.failing = false;
  timers.fire();
  sockets[1].open();
  sockets[1].receive({ type: 'snapshot', revision: 4, agents: [agent('a'), agent('b')], generated_at_unix_ms: 2 });
  assert.deepEqual(state.agents.map(item => item.id), ['a', 'b']);
  assert.deepEqual(connections, ['connecting', 'live', 'reconnecting', 'live']);
});

test('a closed socket reports reconnecting, keeps the devices, and reconnects within 1 s', () => {
  const { sockets, state, connections, timers } = harness();
  sockets[0].open();
  sockets[0].receive({ type: 'snapshot', revision: 1, agents: [agent('a')], generated_at_unix_ms: 1 });
  sockets[0].close(1001, 'going away');
  assert.deepEqual(connections, ['connecting', 'live', 'reconnecting']);
  assert.deepEqual(state.agents.map(item => item.id), ['a']);
  assert.deepEqual(timers.delays, [750]);
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

test('each socket as it opens is told what the page watches, and a closed one is sent nothing', () => {
  const calls = [];
  let send = null;
  const watch = {
    attach: next => { calls.push('attach'); send = next; },
    detach: () => { calls.push('detach'); },
  };
  const { sockets, timers } = harness({ watch });
  assert.deepEqual(calls, [], 'not before it opens');
  sockets[0].open();
  assert.deepEqual(calls, ['attach']);
  send(['a', 'b']);
  assert.deepEqual(sockets[0].sent, [watchMessage(['a', 'b'])]);
  assert.equal(sockets[0].sent[0], '{"type":"watch","devices":["a","b"]}');
  sockets[0].close(1006);
  assert.deepEqual(calls, ['attach', 'detach']);
  const stale = send;
  timers.fire();
  sockets[1].open();
  assert.deepEqual(calls, ['attach', 'detach', 'attach'], 'the new socket watches nothing until sent the set');
  stale(['c']);
  assert.deepEqual(sockets[0].sent.length, 1, 'the closed socket is sent nothing');
  assert.deepEqual(sockets[1].sent, []);
  send(['a', 'b']);
  assert.deepEqual(sockets[1].sent, [watchMessage(['a', 'b'])]);
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

test('alert events arrive beside the revisions without disturbing them', () => {
  const { sockets, state, revision } = harness();
  sockets[0].open();
  sockets[0].receive({ type: 'snapshot', revision: 3, agents: [agent('a')], generated_at_unix_ms: 0 });
  const counts = { open: 1, acknowledged: 0, open_info: 0, open_warning: 1, open_critical: 0 };
  sockets[0].receive({ type: 'alert_counts', counts });
  const alert = { id: 'x', rule_id: 'r', device_id: 'a', state: 'open', raised_at: 5 };
  sockets[0].receive({ type: 'alert_upsert', alert, counts });
  assert.deepEqual(state.alerts, [{ type: 'alert_counts', counts }, { type: 'alert_upsert', alert, counts }]);
  assert.equal(revision.current, 3);
  assert.deepEqual(sockets[0].sent, [], 'no snapshot is requested');
});

test('job events arrive beside the revisions without disturbing them', () => {
  const { sockets, state, revision } = harness();
  sockets[0].open();
  sockets[0].receive({ type: 'snapshot', revision: 2, agents: [agent('a')], generated_at_unix_ms: 0 });
  const job = { id: 'j', item_name: 'Clean up', counts: { total: 1, running: 1 } };
  sockets[0].receive({ type: 'job_update', job });
  sockets[0].receive({ type: 'jobs_resync' });
  assert.deepEqual(state.jobs, [{ type: 'job_update', job }, { type: 'jobs_resync' }]);
  assert.equal(revision.current, 2);
  assert.deepEqual(sockets[0].sent, [], 'no snapshot is requested');
});

test('update events arrive beside the revisions without disturbing them', () => {
  const { sockets, state, revision } = harness();
  sockets[0].open();
  sockets[0].receive({ type: 'snapshot', revision: 2, agents: [agent('a')], generated_at_unix_ms: 0 });
  const device = { device_id: 'a', pending: 3 };
  const run = { id: 'r', device_id: 'a', kind: 'install', status: 'pending' };
  sockets[0].receive({ type: 'device_updates', device });
  sockets[0].receive({ type: 'update_run', run });
  sockets[0].receive({ type: 'updates_resync' });
  assert.deepEqual(state.updates, [{ type: 'device_updates', device }, { type: 'update_run', run }, { type: 'updates_resync' }]);
  assert.equal(revision.current, 2);
  assert.deepEqual(sockets[0].sent, [], 'no snapshot is requested');
});
