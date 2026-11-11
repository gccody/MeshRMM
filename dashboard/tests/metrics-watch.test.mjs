import assert from "node:assert/strict";
import test from "node:test";
import { DeviceStore } from "../features/agents/device-store.ts";
import { MAX_WATCHED, WATCH_INTERVAL_MS, metricsWatch } from "../features/agents/metrics-watch.ts";

// Firing the due timers moves the clock to them.
function fakeTimers() {
  const pending = new Set();
  let clock = 0;
  return {
    pending,
    now: () => clock,
    setTimeout(callback, ms) {
      const timer = { callback, at: clock + ms };
      pending.add(timer);
      return timer;
    },
    clearTimeout(timer) {
      pending.delete(timer);
    },
    advance(ms) {
      clock += ms;
      for (const timer of [...pending].sort((left, right) => left.at - right.at)) {
        if (timer.at > clock) continue;
        pending.delete(timer);
        timer.callback();
      }
    },
  };
}

// A store whose readers are the tiles a page has mounted, the watch that
// follows them, and what each socket was sent.
function harness({ visible } = {}) {
  const timers = fakeTimers();
  const devices = new DeviceStore({ schedule: () => {} });
  const watched = [];
  const added = [];
  const watch = metricsWatch({
    shown: () => devices.readers(),
    onWatching: (set, newly) => {
      watched.push([...set].sort());
      added.push([...newly].sort());
      devices.watching(set);
    },
    visible,
    timers,
  });
  devices.subscribeReaders(() => watch.changed());
  const sockets = [];
  const open = () => {
    const sent = [];
    sockets.push(sent);
    watch.attach((ids) => sent.push([...ids].sort()));
    return sent;
  };
  // A tile in view, until its returned function unmounts it.
  const show = (id) => devices.subscribeReading(id, () => {});
  return { timers, devices, watch, watched, added, open, show };
}

test("the tiles in view are watched, and only they", () => {
  const { timers, open, show, watched } = harness();
  const unmount = Object.fromEntries(["a", "b", "c"].map((id) => [id, show(id)]));
  const sent = open();
  assert.deepEqual(sent, [], "sent once the render that opened it is done");
  timers.advance(0);
  assert.deepEqual(sent, [["a", "b", "c"]]);
  assert.deepEqual(watched, [["a", "b", "c"]]);
  // Scrolled: a leaves view, d comes in.
  unmount.a();
  show("d");
  timers.advance(WATCH_INTERVAL_MS);
  assert.deepEqual(sent.at(-1), ["b", "c", "d"]);
});

test("a page that shows no reading watches nothing and sends nothing", () => {
  const { timers, open } = harness();
  const sent = open();
  timers.advance(10_000);
  assert.deepEqual(sent, []);
});

test("a run of changes sends one message, at most every 250 ms", () => {
  const { timers, open, show } = harness();
  const sent = open();
  show("first");
  timers.advance(0);
  assert.equal(sent.length, 1);
  // A fast scroll: tiles mount and unmount many times within the interval.
  for (let row = 0; row < 40; row++) {
    const gone = show(`row-${row}`);
    timers.advance(5);
    if (row < 39) gone();
  }
  assert.equal(sent.length, 1, "nothing before the interval is up");
  timers.advance(WATCH_INTERVAL_MS);
  assert.deepEqual(sent, [["first"], ["first", "row-39"]]);
  // A change undone before it is sent sends nothing.
  const flicker = show("flicker");
  flicker();
  timers.advance(WATCH_INTERVAL_MS);
  assert.equal(sent.length, 2);
});

test("a hidden tab watches nothing and asks again for what it shows when shown", () => {
  const { timers, open, show, watch, devices } = harness();
  show("a");
  show("b");
  const sent = open();
  timers.advance(0);
  devices.replace({ agents: [{ id: "a", name: "A", platform: "windows", connected: true }], companies: [], tags: [] });
  devices.applyReadings([{ device_id: "a", at: 1, cpu_percent: 1, memory_used_bytes: 1, memory_total_bytes: 2, network_received_bytes_per_second: 0, network_sent_bytes_per_second: 0, uptime_seconds: 1, volumes: [] }]);
  assert.ok(devices.reading("a"));
  watch.setVisible(false);
  timers.advance(WATCH_INTERVAL_MS);
  assert.deepEqual(sent, [["a", "b"], []]);
  assert.equal(devices.reading("a"), undefined, "a hidden tab's readings would be old when it is shown");
  // Scrolling a hidden tab, as a resize may, changes nothing it watches.
  show("c");
  timers.advance(WATCH_INTERVAL_MS);
  assert.equal(sent.length, 2);
  watch.setVisible(true);
  timers.advance(WATCH_INTERVAL_MS);
  assert.deepEqual(sent.at(-1), ["a", "b", "c"]);
});

test("a tab opened hidden watches nothing until shown", () => {
  const { timers, open, show, watch } = harness({ visible: false });
  show("a");
  const sent = open();
  timers.advance(1_000);
  assert.deepEqual(sent, []);
  watch.setVisible(true);
  timers.advance(0);
  assert.deepEqual(sent, [["a"]]);
});

test("each new socket is sent the set as it opens, and none while there is none", () => {
  const { timers, open, show, watch } = harness();
  show("a");
  const first = open();
  timers.advance(0);
  watch.detach();
  // While reconnecting, the page changes.
  show("b");
  timers.advance(10_000);
  assert.deepEqual(first, [["a"]], "the closed socket is sent nothing");
  const second = open();
  timers.advance(0);
  assert.deepEqual(second, [["a", "b"]], "without waiting out the interval");
  // A socket closed before its set was sent is not sent it later.
  watch.detach();
  open();
  watch.detach();
  assert.equal(timers.pending.size, 0);
});

test("at most the server's bound is asked for", () => {
  const { timers, open, show } = harness();
  for (let n = 0; n < MAX_WATCHED + 100; n++) show(`d${n}`);
  const sent = open();
  timers.advance(0);
  assert.equal(sent[0].length, MAX_WATCHED);
});

test("each change names the devices the socket did not watch before, and a new socket all of them", () => {
  const { timers, open, show, watch, added } = harness();
  show("a");
  const hideB = show("b");
  open();
  timers.advance(0);
  show("c");
  hideB();
  timers.advance(WATCH_INTERVAL_MS);
  assert.deepEqual(added, [["a", "b"], ["c"]]);
  // Hidden and shown again, its images may have changed unannounced.
  watch.setVisible(false);
  timers.advance(WATCH_INTERVAL_MS);
  watch.setVisible(true);
  timers.advance(WATCH_INTERVAL_MS);
  assert.deepEqual(added.slice(2), [[], ["a", "c"]]);
  // As after a reconnect: the new socket missed what the old one heard.
  watch.detach();
  open();
  timers.advance(WATCH_INTERVAL_MS);
  assert.deepEqual(added.at(-1), ["a", "c"]);
});
