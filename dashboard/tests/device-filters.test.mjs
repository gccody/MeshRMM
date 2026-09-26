import assert from "node:assert/strict";
import test from "node:test";
import {
  DEFAULT_DEVICE_FILTERS,
  MAX_DEVICE_QUERY_LENGTH,
  filterAgents,
  parseDeviceFilters,
  serializeDeviceFilters,
} from "../features/agents/device-filters.ts";

test("parses the defaults from an empty or missing search", () => {
  assert.deepEqual(parseDeviceFilters(new URLSearchParams()), DEFAULT_DEVICE_FILTERS);
  assert.deepEqual(parseDeviceFilters(null), { query: "", status: "all" });
});

test("reads the query and status, falling back to all for an unknown status", () => {
  assert.deepEqual(parseDeviceFilters(new URLSearchParams("q=alpha&status=offline")), { query: "alpha", status: "offline" });
  assert.deepEqual(parseDeviceFilters(new URLSearchParams("status=online")), { query: "", status: "online" });
  for (const status of ["ONLINE", "idle", "", "all"]) {
    assert.equal(parseDeviceFilters(new URLSearchParams({ status })).status, "all", status);
  }
});

test("caps a long query", () => {
  const query = "x".repeat(MAX_DEVICE_QUERY_LENGTH + 50);
  assert.equal(parseDeviceFilters(new URLSearchParams({ q: query })).query.length, MAX_DEVICE_QUERY_LENGTH);
  assert.equal(serializeDeviceFilters({ query, status: "all" }), `?q=${"x".repeat(MAX_DEVICE_QUERY_LENGTH)}`);
});

test("serialization omits defaults and encodes reserved characters", () => {
  assert.equal(serializeDeviceFilters(DEFAULT_DEVICE_FILTERS), "");
  assert.equal(serializeDeviceFilters({ query: "", status: "online" }), "?status=online");
  assert.equal(serializeDeviceFilters({ query: "alpha", status: "all" }), "?q=alpha");
  const search = serializeDeviceFilters({ query: "a&b=c #?/+%", status: "offline" });
  assert.equal(search, "?q=a%26b%3Dc+%23%3F%2F%2B%25&status=offline");
  assert.deepEqual(parseDeviceFilters(new URLSearchParams(search)), { query: "a&b=c #?/+%", status: "offline" });
});

test("filters by case-insensitive name or ID and by connection status", () => {
  const agents = [
    { id: "dev-001", name: "Front Desk", connected: true },
    { id: "dev-002", name: "Warehouse PC", connected: false },
    { id: "LAB-9", name: "Lab", connected: true },
  ];
  const ids = (filters) => filterAgents(agents, filters).map((agent) => agent.id);
  assert.deepEqual(ids(DEFAULT_DEVICE_FILTERS), ["dev-001", "dev-002", "LAB-9"]);
  assert.deepEqual(ids({ query: "front DESK", status: "all" }), ["dev-001"]);
  assert.deepEqual(ids({ query: "lab-9", status: "all" }), ["LAB-9"]);
  assert.deepEqual(ids({ query: "  DEV-00 ", status: "all" }), ["dev-001", "dev-002"]);
  assert.deepEqual(ids({ query: "", status: "online" }), ["dev-001", "LAB-9"]);
  assert.deepEqual(ids({ query: "dev", status: "offline" }), ["dev-002"]);
  assert.deepEqual(ids({ query: "nothing", status: "all" }), []);
});
