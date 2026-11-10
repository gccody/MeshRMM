import assert from "node:assert/strict";
import test from "node:test";
import { reloadGate } from "../lib/reload-gate.ts";

const flush = async () => { for (let i = 0; i < 10; i++) await Promise.resolve(); };

// A read that finishes when `finish` is called, counting how many started
// and how many ran at once.
function reader() {
  const state = { started: 0, running: 0, mostAtOnce: 0, pending: [] };
  state.reload = () => {
    state.started++;
    state.running++;
    state.mostAtOnce = Math.max(state.mostAtOnce, state.running);
    return new Promise((resolve) => state.pending.push(() => {
      state.running--;
      resolve();
    }));
  };
  state.finish = async () => {
    state.pending.splice(0).forEach((done) => done());
    await flush();
  };
  return state;
}

test("a burst of asks becomes one read at the end of the interval, and a later ask one more", async (t) => {
  t.mock.timers.enable({ apis: ["setTimeout"] });
  const read = reader();
  const gate = reloadGate({ reload: read.reload, intervalMs: 2_000, hidden: () => false });
  for (let i = 0; i < 500; i++) {
    gate.request();
    t.mock.timers.tick(3);
  }
  await flush();
  assert.equal(read.started, 0, "nothing before the interval since the first ask");
  t.mock.timers.tick(500);
  await flush();
  assert.equal(read.started, 1, "500 asks, one read");
  await read.finish();
  t.mock.timers.tick(10_000);
  await flush();
  assert.equal(read.started, 1, "no ask, no read");

  gate.request();
  t.mock.timers.tick(2_000);
  await flush();
  assert.equal(read.started, 2, "a later ask reads once more");
  await read.finish();
  assert.equal(read.mostAtOnce, 1);
  gate.stop();
});

test("asks while a read runs wait for it, then become one read", async (t) => {
  t.mock.timers.enable({ apis: ["setTimeout"] });
  const read = reader();
  const gate = reloadGate({ reload: read.reload, intervalMs: 1_000, hidden: () => false });
  gate.request();
  t.mock.timers.tick(1_000);
  await flush();
  assert.equal(read.started, 1);
  // The read is slow, and asks keep coming.
  for (let i = 0; i < 50; i++) {
    gate.request();
    t.mock.timers.tick(100);
  }
  await flush();
  assert.equal(read.started, 1, "no second read while the first runs");
  await read.finish();
  t.mock.timers.tick(999);
  await flush();
  assert.equal(read.started, 1, "the next waits the interval after the first finished");
  t.mock.timers.tick(1);
  await flush();
  assert.equal(read.started, 2);
  await read.finish();
  assert.equal(read.mostAtOnce, 1, "never two reads at once");
  gate.stop();
});

test("a failed read doesn't stop later ones", async (t) => {
  t.mock.timers.enable({ apis: ["setTimeout"] });
  let started = 0;
  const gate = reloadGate({ reload: async () => { started++; throw new Error("offline"); }, intervalMs: 1_000, hidden: () => false });
  gate.request();
  t.mock.timers.tick(1_000);
  await flush();
  gate.request();
  t.mock.timers.tick(1_000);
  await flush();
  assert.equal(started, 2);
  gate.stop();
});

test("nothing is read while hidden, and once shown one read catches up", async (t) => {
  t.mock.timers.enable({ apis: ["setTimeout"] });
  const read = reader();
  let hidden = true;
  const gate = reloadGate({ reload: read.reload, intervalMs: 1_000, hidden: () => hidden });
  for (let i = 0; i < 100; i++) gate.request();
  t.mock.timers.tick(60_000);
  await flush();
  assert.equal(read.started, 0);
  hidden = false;
  gate.shown();
  gate.shown();
  t.mock.timers.tick(1_000);
  await flush();
  assert.equal(read.started, 1, "one read for everything asked while hidden");
  await read.finish();

  // Hidden after the ask, before the read was due: it waits to be shown.
  gate.request();
  hidden = true;
  t.mock.timers.tick(1_000);
  await flush();
  assert.equal(read.started, 1);
  hidden = false;
  gate.shown();
  t.mock.timers.tick(1_000);
  await flush();
  assert.equal(read.started, 2);
  await read.finish();

  // Shown with nothing asked: nothing read.
  gate.shown();
  t.mock.timers.tick(1_000);
  await flush();
  assert.equal(read.started, 2);
  gate.stop();
});

test("a stopped gate reads nothing more", async (t) => {
  t.mock.timers.enable({ apis: ["setTimeout"] });
  const read = reader();
  const gate = reloadGate({ reload: read.reload, intervalMs: 1_000, hidden: () => false });
  gate.request();
  gate.stop();
  gate.request();
  t.mock.timers.tick(5_000);
  await flush();
  assert.equal(read.started, 0);
});
