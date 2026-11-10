import assert from "node:assert/strict";
import test from "node:test";
import { DeviceStore } from "../features/agents/device-store.ts";
import { compareAgents } from "../features/agents/model.ts";

// How the list was sorted before the store kept it: the order it must keep.
const sortAgents = (items) =>
  [...items].sort(
    (left, right) =>
      Number(right.connected) - Number(left.connected) ||
      left.name.localeCompare(right.name) ||
      left.id.localeCompare(right.id),
  );

// A frame scheduler the test runs by hand.
function frames() {
  const pending = [];
  return {
    schedule: (flush) => pending.push(flush),
    get pending() { return pending.length; },
    run() {
      const due = pending.splice(0);
      for (const flush of due) flush();
    },
  };
}

// Counts the comparisons the store makes.
function counted() {
  const counter = { comparisons: 0 };
  counter.compare = (left, right) => {
    counter.comparisons++;
    return compareAgents(left, right);
  };
  return counter;
}

// Deterministic, so a failure can be replayed.
function seeded(seed) {
  let state = seed >>> 0;
  return () => {
    state = (state + 0x6d2b79f5) >>> 0;
    let value = state;
    value = Math.imul(value ^ (value >>> 15), value | 1);
    value ^= value + Math.imul(value ^ (value >>> 7), value | 61);
    return ((value ^ (value >>> 14)) >>> 0) / 4_294_967_296;
  };
}

// Names that collate differently from their lowercased bytes, as the server
// sorts them, and that tie, so the ID decides.
const NAMES = ["alpha", "Alpha", "ALPHA", "Ébène", "ebene", "zulu", "Zulu", "_build", "build-2", "build10", "build9", "ñandú", "Øst", "oslo", "名前", "42"];

const device = (id, name, connected) => ({ id, name, platform: "windows", connected });
const empty = { companies: [], tags: [] };

test("keeps the list in the order sorting it from scratch gives, through random changes", () => {
  for (const seed of [1, 2, 3, 4, 5]) {
    const random = seeded(seed);
    const pick = (items) => items[Math.floor(random() * items.length)];
    const ids = Array.from({ length: 300 }, (_, index) => `d${index.toString(16)}`);
    const clock = frames();
    const devices = new DeviceStore({ schedule: clock.schedule });
    const model = new Map();
    for (const id of ids.slice(0, 150)) model.set(id, device(id, pick(NAMES), random() < 0.5));
    devices.replace({ agents: [...model.values()], ...empty });
    for (let step = 0; step < 2_000; step++) {
      const id = pick(ids);
      const roll = random();
      const current = model.get(id);
      if (roll < 0.15) {
        model.delete(id);
        devices.apply({ type: "agent_deleted", revision: step, agent_id: id });
      } else if (roll < 0.4 && current) {
        // Online or offline.
        const next = { ...current, connected: !current.connected };
        model.set(id, next);
        devices.apply({ type: "agent_upsert", revision: step, agent: next });
      } else if (roll < 0.6 && current) {
        // A change that leaves it where it is.
        const next = { ...current, tag_ids: [String(step)] };
        model.set(id, next);
        devices.apply({ type: "agent_upsert", revision: step, agent: next });
      } else {
        // A new device, or a renamed one.
        const next = device(id, pick(NAMES), current?.connected ?? random() < 0.5);
        model.set(id, next);
        devices.apply({ type: "agent_upsert", revision: step, agent: next });
      }
      // Read often at first, so devices move one at a time, then rarely, so
      // many changed devices are merged in at once.
      if (random() < (step < 1_000 ? 0.3 : 0.002) || step % 250 === 249) {
        const expected = sortAgents([...model.values()]);
        assert.deepEqual(devices.agents(), expected, `seed ${seed}, step ${step}`);
        const online = expected.filter((agent) => agent.connected).length;
        assert.deepEqual(devices.counts(), { total: expected.length, online }, `seed ${seed}, step ${step}`);
      }
      if (random() < 0.1) clock.run();
    }
  }
});

test("a snapshot is sorted in the browser's order, not the server's", () => {
  const devices = new DeviceStore({ schedule: () => {} });
  // The server's order: by lowercased name, byte by byte.
  const agents = [device("1", "Zulu", true), device("2", "_build", true), device("3", "Ébène", true), device("4", "zebra", false)];
  devices.replace({ agents, ...empty });
  assert.deepEqual(devices.agents(), sortAgents(agents));
  assert.deepEqual(devices.counts(), { total: 4, online: 3 });
});

test("a snapshot already in the list's order costs one comparison a device; one out of order is still sorted", () => {
  const count = 5_000;
  const inOrder = sortAgents(Array.from({ length: count }, (_, index) => device(`id-${index}`, `device ${index}`, index % 3 !== 0)));
  const counter = counted();
  const devices = new DeviceStore({ schedule: () => {}, compare: counter.compare });
  devices.replace({ agents: inOrder, ...empty });
  assert.deepEqual(devices.agents(), inOrder);
  assert.equal(counter.comparisons, count - 1, "checked, not sorted again");
  // Where the server's order and the browser's differ: by case, then ID.
  const outOfOrder = [...inOrder, device("b", "zz", true), device("a", "ZZ", true)];
  counter.comparisons = 0;
  devices.replace({ agents: outOfOrder, ...empty });
  assert.deepEqual(devices.agents(), sortAgents(outOfOrder));
  assert.notDeepEqual(devices.agents(), outOfOrder);
});

test("a snapshot replaces everything", () => {
  const clock = frames();
  const devices = new DeviceStore({ schedule: clock.schedule });
  devices.replace({ agents: [device("a", "A", true), device("b", "B", true)], companies: [{ id: "c", name: "Acme" }], tags: [{ id: "t", name: "Servers", color: "blue" }] });
  devices.applyReadings([reading("a", 10), reading("b", 20)]);
  devices.replace({ agents: [device("b", "B", false), device("c", "C", true)], companies: [], tags: [] });
  assert.deepEqual(devices.agents().map((agent) => agent.id), ["c", "b"]);
  assert.equal(devices.get("a"), undefined);
  assert.deepEqual(devices.counts(), { total: 2, online: 1 });
  assert.deepEqual(devices.companies(), []);
  assert.deepEqual(devices.tags(), []);
  assert.equal(devices.reading("a"), undefined, "a device gone takes its reading");
  assert.equal(devices.reading("b"), undefined, "an offline device has none");
});

test("ten thousand changes in one frame notify once and cost a merge, not a sort each", () => {
  const counter = counted();
  const clock = frames();
  const devices = new DeviceStore({ schedule: clock.schedule, compare: counter.compare });
  const count = 10_000;
  const offline = Array.from({ length: count }, (_, index) => device(`id-${index}`, `device ${index}`, false));
  devices.replace({ agents: offline, ...empty });
  devices.agents();
  clock.run();
  let notified = 0;
  let counts = 0;
  devices.subscribe("agents", () => notified++);
  devices.subscribe("ids", () => assert.fail("no device was added or removed"));
  const unsubscribe = devices.subscribe("agents", () => counts++);
  counter.comparisons = 0;

  // The fleet reconnects after a server restart.
  for (const agent of offline) devices.apply({ type: "agent_upsert", revision: 1, agent: { ...agent, connected: true } });
  assert.equal(counter.comparisons, 0, "nothing is sorted until the list is read");
  assert.equal(clock.pending, 1, "one notification is due");
  clock.run();
  assert.equal(notified, 1);
  assert.equal(counts, 1);
  unsubscribe();

  const list = devices.agents();
  assert.equal(list.length, count);
  assert.ok(list.every((agent) => agent.connected));
  assert.deepEqual(list, sortAgents(list));
  // Sorting the changed devices and merging them in: about n log n once,
  // where re-sorting the list on each change would be ten thousand times that.
  assert.ok(counter.comparisons < count * 20, `${counter.comparisons} comparisons`);
  assert.equal(devices.agents(), list, "reading again does no work");
  assert.deepEqual(devices.counts(), { total: count, online: count });
});

test("one change moves one device", () => {
  const counter = counted();
  const devices = new DeviceStore({ schedule: () => {}, compare: counter.compare });
  devices.replace({ agents: Array.from({ length: 10_000 }, (_, index) => device(`id-${index}`, `device ${index}`, index % 2 === 0)), ...empty });
  devices.agents();
  counter.comparisons = 0;
  devices.apply({ type: "agent_upsert", revision: 1, agent: device("id-7", "device 7", true) });
  const list = devices.agents();
  assert.ok(counter.comparisons < 50, `${counter.comparisons} comparisons`);
  assert.deepEqual(list, sortAgents(list));
  assert.deepEqual(devices.counts(), { total: 10_000, online: 5_001 });
});

const reading = (id, cpu) => ({
  device_id: id,
  at: 1,
  cpu_percent: cpu,
  memory_used_bytes: 1,
  memory_total_bytes: 2,
  network_received_bytes_per_second: 0,
  network_sent_bytes_per_second: 0,
  uptime_seconds: 1,
  volumes: [],
});

test("a reading notifies only its device's subscribers", () => {
  const clock = frames();
  const devices = new DeviceStore({ schedule: clock.schedule });
  devices.replace({ agents: [device("a", "A", true), device("b", "B", true), device("c", "C", true)], ...empty });
  clock.run();
  const heard = [];
  for (const id of ["a", "b", "c"]) devices.subscribeReading(id, () => heard.push(id));
  for (const topic of ["agents", "ids", "companies", "tags", "meta"]) devices.subscribe(topic, () => heard.push(topic));
  devices.subscribeDevice("a", () => heard.push("device a"));
  const before = devices.agents();

  devices.applyReadings([reading("a", 10), reading("c", 30), reading("unknown", 50)]);
  clock.run();
  assert.deepEqual(heard.sort(), ["a", "c"]);
  assert.equal(devices.reading("a").cpu_percent, 10);
  assert.equal(devices.reading("b"), undefined);
  assert.equal(devices.reading("unknown"), undefined, "an unknown device's reading is not kept");
  assert.equal(devices.agents(), before, "the list is untouched");

  // A message naming no one followed notifies no one.
  heard.length = 0;
  devices.applyReadings([]);
  assert.equal(clock.pending, 0);
});

test("a device going offline or deleted takes its reading", () => {
  const clock = frames();
  const devices = new DeviceStore({ schedule: clock.schedule });
  devices.replace({ agents: [device("a", "A", true), device("b", "B", true)], ...empty });
  devices.applyReadings([reading("a", 10), reading("b", 20)]);
  clock.run();
  let heard = 0;
  devices.subscribeReading("a", () => heard++);
  devices.apply({ type: "agent_upsert", revision: 1, agent: device("a", "A", false) });
  devices.apply({ type: "agent_deleted", revision: 2, agent_id: "b" });
  clock.run();
  assert.equal(heard, 1);
  assert.equal(devices.reading("a"), undefined);
  assert.equal(devices.reading("b"), undefined);
  devices.applyReadings([reading("a", 30)]);
  assert.equal(devices.reading("a"), undefined, "an offline device's reading is not kept");
});

test("a device's subscribers hear of its changes only", () => {
  const clock = frames();
  const devices = new DeviceStore({ schedule: clock.schedule });
  devices.replace({ agents: [device("a", "A", true), device("b", "B", true)], ...empty });
  clock.run();
  const heard = [];
  devices.subscribeDevice("a", () => heard.push("a"));
  devices.subscribeDevice("b", () => heard.push("b"));
  devices.apply({ type: "agent_upsert", revision: 1, agent: device("a", "A2", true) });
  devices.apply({ type: "agent_upsert", revision: 2, agent: device("a", "A3", true) });
  clock.run();
  assert.deepEqual(heard, ["a"]);
  assert.equal(devices.get("a").name, "A3");
  devices.apply({ type: "agent_deleted", revision: 3, agent_id: "missing" });
  assert.equal(clock.pending, 0, "deleting a device the store lacks changes nothing");
});

test("snapshots stay the same between changes", () => {
  const devices = new DeviceStore({ schedule: () => {} });
  devices.replace({ agents: [device("a", "A", true)], companies: [{ id: "c", name: "Acme" }], tags: [] });
  const [agents, counts, companies, tags, updated, agent] = [devices.agents(), devices.counts(), devices.companies(), devices.tags(), devices.lastUpdated(), devices.get("a")];
  assert.equal(devices.agents(), agents);
  assert.equal(devices.counts(), counts);
  assert.equal(devices.companies(), companies);
  assert.equal(devices.tags(), tags);
  assert.equal(devices.lastUpdated(), updated);
  assert.equal(devices.get("a"), agent);
  devices.apply({ type: "tag_upsert", revision: 1, tag: { id: "t", name: "T", color: "blue" } });
  assert.equal(devices.agents(), agents, "a tag leaves the devices alone");
  assert.equal(devices.counts(), counts);
  assert.equal(devices.companies(), companies);
  assert.notEqual(devices.tags(), tags);
  devices.apply({ type: "agent_upsert", revision: 2, agent: device("a", "A", true) });
  assert.notEqual(devices.agents(), agents);
  assert.equal(devices.counts(), counts, "the counts didn't change");
});

test("clearing empties the store and tells every subscriber", () => {
  const clock = frames();
  const devices = new DeviceStore({ schedule: clock.schedule });
  devices.replace({ agents: [device("a", "A", true)], ...empty });
  devices.applyReadings([reading("a", 10)]);
  clock.run();
  const heard = [];
  devices.subscribeDevice("a", () => heard.push("device"));
  devices.subscribeReading("a", () => heard.push("reading"));
  devices.subscribe("meta", () => heard.push("meta"));
  devices.clear();
  clock.run();
  assert.deepEqual(heard.sort(), ["device", "meta", "reading"]);
  assert.equal(devices.loaded(), false);
  assert.equal(devices.lastUpdated(), null);
  assert.deepEqual(devices.agents(), []);
  assert.deepEqual(devices.counts(), { total: 0, online: 0 });
  assert.equal(devices.reading("a"), undefined);
});

test("a hidden tab, which runs no frames, is notified by a timer instead", async () => {
  const frameRequests = [];
  globalThis.requestAnimationFrame = (callback) => frameRequests.push(callback);
  globalThis.cancelAnimationFrame = () => {};
  try {
    const devices = new DeviceStore();
    let heard = 0;
    devices.subscribe("agents", () => heard++);
    devices.replace({ agents: [device("a", "A", true)], ...empty });
    devices.apply({ type: "agent_upsert", revision: 1, agent: device("b", "B", true) });
    assert.equal(frameRequests.length, 1, "one frame is asked for");
    await new Promise((resolve) => setTimeout(resolve, 400));
    assert.equal(heard, 1, "the timer notified once");
    // The frame arriving late, when the tab is shown, does nothing more.
    frameRequests[0]();
    assert.equal(heard, 1);
  } finally {
    delete globalThis.requestAnimationFrame;
    delete globalThis.cancelAnimationFrame;
  }
});
