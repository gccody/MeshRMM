import assert from "node:assert/strict";
import test from "node:test";
import {
  appendLivePoint,
  areaPath,
  formatBitRate,
  formatBytes,
  formatPercent,
  formatUptime,
  linePath,
  nearestPoint,
  niceCeiling,
  parseDeviceMetrics,
  parseMetricsEvent,
  percentOf,
  pointFromReading,
  storageOf,
  usageTone,
} from "../features/metrics/model.ts";

const reading = (at, cpu = 10) => ({
  device_id: "a",
  at,
  cpu_percent: cpu,
  memory_used_bytes: 4,
  memory_total_bytes: 16,
  network_received_bytes_per_second: 100,
  network_sent_bytes_per_second: 50,
  uptime_seconds: 90_000,
  volumes: [
    { name: "C:", total_bytes: 100, free_bytes: 40 },
    { name: "D:", total_bytes: 50, free_bytes: 50 },
  ],
});

test("parses the event socket's readings and skips invalid ones", () => {
  assert.deepEqual(parseMetricsEvent({ type: "metrics", readings: [reading(1), { device_id: "b", at: -1 }] }), [reading(1)]);
  assert.equal(parseMetricsEvent({ type: "snapshot", revision: 1, agents: [] }), null);
  assert.equal(parseMetricsEvent(null), null);
});

test("parses a device's metrics response", () => {
  const point = pointFromReading(reading(5));
  const parsed = parseDeviceMetrics({ range: "hour", step_ms: 60_000, latest: reading(5), points: [point] });
  assert.equal(parsed.range, "hour");
  assert.deepEqual(parsed.points, [point]);
  assert.equal(parseDeviceMetrics({ range: "hour", step_ms: 60_000, latest: null, points: [] }).latest, null);
  assert.equal(parseDeviceMetrics({ range: "year", step_ms: 1, latest: null, points: [] }), null);
  assert.equal(parseDeviceMetrics({ range: "live", step_ms: 1, latest: { device_id: "a" }, points: [] }), null);
});

test("a reading becomes a point with its volumes' storage summed", () => {
  assert.deepEqual(storageOf(reading(1).volumes), { used: 60, total: 150 });
  const point = pointFromReading(reading(7, 33));
  assert.equal(point.at, 7);
  assert.equal(point.cpu_percent_max, 33);
  assert.equal(point.storage_used_bytes, 60);
  assert.equal(point.storage_total_bytes, 150);
});

test("live points grow with newer readings inside the window", () => {
  let points = [];
  points = appendLivePoint(points, reading(1_000), 10_000);
  points = appendLivePoint(points, reading(6_000), 10_000);
  assert.equal(appendLivePoint(points, reading(6_000), 10_000), points, "a repeated reading is ignored");
  points = appendLivePoint(points, reading(12_000), 10_000);
  assert.deepEqual(points.map((point) => point.at), [6_000, 12_000]);
});

test("formats sizes, rates, loads and uptime", () => {
  assert.equal(formatBytes(512), "512 B");
  assert.equal(formatBytes(16 * 1024 ** 3), "16.0 GB");
  assert.equal(formatBytes(476 * 1024 ** 3), "476 GB");
  assert.equal(formatBitRate(0), "0 bps");
  assert.equal(formatBitRate(1_550_000 / 8), "1.6 Mbps");
  assert.equal(formatPercent(0), "0%");
  assert.equal(formatPercent(0.4), "<1%");
  assert.equal(formatPercent(99.6), "100%");
  assert.equal(formatUptime(90_000), "1 d 1 h");
  assert.equal(formatUptime(3_720), "1 h 2 min");
  assert.equal(formatUptime(59), "0 min");
  assert.equal(percentOf(5, 0), 0);
  assert.equal(percentOf(3, 4), 75);
  assert.deepEqual([50, 80, 95].map(usageTone), ["normal", "high", "critical"]);
});

test("axis maxima are round", () => {
  assert.equal(niceCeiling(0), 1);
  assert.equal(niceCeiling(7), 10);
  assert.equal(niceCeiling(1_200), 2_000);
  assert.equal(niceCeiling(3_100_000), 5_000_000);
});

test("lines break where readings are missing, and areas close each piece", () => {
  const points = [0, 10, 20, 100, 110].map((at) => pointFromReading(reading(at, at)));
  const x = (at) => at;
  const y = (value) => 100 - value;
  assert.equal(linePath(points, (point) => point.cpu_percent, x, y, 30), "M0.0,100.0L10.0,90.0L20.0,80.0M100.0,0.0L110.0,-10.0");
  assert.equal(
    areaPath(points.slice(0, 2), (point) => point.cpu_percent, x, y, 30, 100),
    "M0.0,100L0.0,100.0L10.0,90.0L10.0,100Z",
  );
  assert.equal(nearestPoint(points, 64).at, 100);
  assert.equal(nearestPoint([], 1), null);
});
