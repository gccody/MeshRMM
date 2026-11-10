import assert from "node:assert/strict";
import test from "node:test";
import { coveredAgents, jobPreview } from "../features/automation/model.ts";
import {
  DEVICE_MATCH_LIMIT,
  DEVICE_ROW_HEIGHT,
  devicesOn,
  matchDevices,
  moreMatchesText,
  platformCounts,
  searchDevices,
} from "../features/device-picker/model.ts";
import { windowRows } from "../lib/windowing.ts";

const PLATFORMS = ["windows", "macos", "linux"];

// `count` devices named "Device 0" on, on each platform in turn, every
// `onlineEvery`th one online.
const fleet = (count, onlineEvery = 2) =>
  Array.from({ length: count }, (_, index) => ({
    id: `dev-${index}`,
    name: `Device ${index}`,
    platform: PLATFORMS[index % 3],
    connected: index % onlineEvery === 0,
  }));

test("the one-device search matches what the Devices list's search does", () => {
  const system = { os: "Windows 11 Pro", manufacturer: "Dell Inc.", model: "OptiPlex 7090", serial_number: "ABC123", public_ip: null, ip_addresses: ["10.1.2.3"], received_at: 1 };
  const agents = [
    { id: "dev-001", name: "Front Desk", platform: "windows", connected: true, system, tag_ids: ["t1"] },
    { id: "dev-002", name: "Warehouse PC", platform: "windows", connected: false },
    { id: "LAB-9", name: "Lab", platform: "linux", connected: true },
  ];
  const tags = [{ id: "t1", name: "Reception", color: "blue" }];
  const ids = (query) => matchDevices(agents, query, tags).shown.map((agent) => agent.id);
  assert.deepEqual(ids(""), ["dev-001", "dev-002", "LAB-9"]);
  assert.deepEqual(ids("  front DESK "), ["dev-001"]);
  assert.deepEqual(ids("lab-9"), ["LAB-9"]);
  assert.deepEqual(ids("10.1.2"), ["dev-001"]);
  assert.deepEqual(ids("optiplex"), ["dev-001"]);
  assert.deepEqual(ids("reception"), ["dev-001"]);
  assert.deepEqual(ids("nothing"), []);
  // An empty search is the list itself, not a copy.
  assert.equal(searchDevices(agents, " ", tags), agents);
});

test("the one-device menu lists the first matches and says how many more there are", () => {
  const agents = fleet(10_000);
  const all = matchDevices(agents, "", []);
  assert.equal(all.shown.length, DEVICE_MATCH_LIMIT);
  assert.equal(all.total, 10_000);
  assert.deepEqual(all.shown.map((agent) => agent.id), agents.slice(0, DEVICE_MATCH_LIMIT).map((agent) => agent.id));
  assert.equal(moreMatchesText(all.shown.length, all.total), `Showing 50 of ${(10_000).toLocaleString()}. Type to narrow them.`);

  // "Device 9999", and the 11 others from "Device 999" up.
  const narrow = matchDevices(agents, "device 999", []);
  assert.equal(narrow.total, 11);
  assert.deepEqual(narrow.shown.map((agent) => agent.name), ["Device 999", ...Array.from({ length: 10 }, (_, index) => `Device 999${index}`)]);
  assert.equal(moreMatchesText(narrow.shown.length, narrow.total), null);

  // A device far down the list is found by its ID.
  assert.deepEqual(matchDevices(agents, "dev-9876", []).shown.map((agent) => agent.id), ["dev-9876"]);
  assert.deepEqual(matchDevices(agents, "Device 1", [], 3).shown.map((agent) => agent.name), ["Device 1", "Device 10", "Device 11"]);
});

test("the many-device list draws only the rows on screen", () => {
  const viewport = 260;
  assert.deepEqual(windowRows(0, viewport, 100_000, DEVICE_ROW_HEIGHT), { first: 0, last: 15 });
  assert.deepEqual(windowRows(DEVICE_ROW_HEIGHT * 50_000, viewport, 100_000, DEVICE_ROW_HEIGHT), { first: 49_990, last: 50_015 });
  assert.deepEqual(windowRows(DEVICE_ROW_HEIGHT * 99_996, viewport, 100_000, DEVICE_ROW_HEIGHT), { first: 99_986, last: 100_000 });
  assert.deepEqual(windowRows(0, viewport, 3, DEVICE_ROW_HEIGHT), { first: 0, last: 3 });
  assert.deepEqual(windowRows(0, viewport, 0, DEVICE_ROW_HEIGHT), { first: 0, last: 0 });
});

test("a deployment lists the devices on its platforms and those chosen already", () => {
  const agents = fleet(10_000);
  const chosen = agents.filter((_, index) => index % 7 === 0).map((agent) => agent.id);
  const expected = agents.filter((agent) => agent.platform === "linux" || chosen.includes(agent.id));
  assert.deepEqual(devicesOn(agents, ["linux"], chosen), expected);
  assert.deepEqual(devicesOn(agents, ["windows", "macos", "linux"]), agents);
  assert.deepEqual(devicesOn(agents, [], ["dev-3", "gone"]).map((agent) => agent.id), ["dev-3"]);

  const covered = agents.filter((_, index) => index % 2 === 1);
  assert.deepEqual(platformCounts(covered, ["linux"]), {
    skipped: covered.filter((agent) => agent.platform !== "linux").length,
    offline: covered.filter((agent) => !agent.connected && agent.platform === "linux").length,
  });
});

test("a job's preview counts the skipped and the offline once each", () => {
  const agents = fleet(10_000, 3);
  const target = { kind: "all", deviceIds: [], companyIds: [] };
  const covered = coveredAgents(agents, target);
  const unsupported = covered.filter((agent) => agent.platform === "windows");
  assert.deepEqual(jobPreview(agents, target, "shell"), {
    devices: 10_000,
    skipped: unsupported.length,
    offline: covered.filter((agent) => !agent.connected && !unsupported.includes(agent)).length,
  });
  assert.deepEqual(jobPreview(agents, target, null), { devices: 10_000, skipped: 0, offline: covered.filter((agent) => !agent.connected).length });
});

// Each of these took seconds when it searched an array for every device.
test("a fleet of 100,000 is counted and listed in one pass", { timeout: 2_000 }, () => {
  const agents = fleet(100_000, 100_001);
  const target = { kind: "all", deviceIds: [], companyIds: [] };
  assert.deepEqual(jobPreview(agents, target, "shell"), { devices: 100_000, skipped: 33_334, offline: 66_666 });
  const everyone = agents.map((agent) => agent.id);
  assert.equal(devicesOn(agents, ["linux"], everyone).length, 100_000);
  assert.deepEqual(platformCounts(agents, ["linux"]), { skipped: 66_667, offline: 33_333 });
});
