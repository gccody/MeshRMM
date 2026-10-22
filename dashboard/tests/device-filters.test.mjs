import assert from "node:assert/strict";
import test from "node:test";
import {
  DEFAULT_DEVICE_FILTERS,
  NO_COMPANY,
  MAX_DEVICE_QUERY_LENGTH,
  filterAgents,
  parseDeviceFilters,
  serializeDeviceFilters,
} from "../features/agents/device-filters.ts";

test("parses the defaults from an empty or missing search", () => {
  assert.deepEqual(parseDeviceFilters(new URLSearchParams()), DEFAULT_DEVICE_FILTERS);
  assert.deepEqual(parseDeviceFilters(null), { query: "", status: "all", company: "" });
});

test("reads the query and status, falling back to all for an unknown status", () => {
  assert.deepEqual(parseDeviceFilters(new URLSearchParams("q=alpha&status=offline")), { query: "alpha", status: "offline", company: "" });
  assert.deepEqual(parseDeviceFilters(new URLSearchParams("status=online")), { query: "", status: "online", company: "" });
  for (const status of ["ONLINE", "idle", "", "all"]) {
    assert.equal(parseDeviceFilters(new URLSearchParams({ status })).status, "all", status);
  }
});

test("caps a long query", () => {
  const query = "x".repeat(MAX_DEVICE_QUERY_LENGTH + 50);
  assert.equal(parseDeviceFilters(new URLSearchParams({ q: query })).query.length, MAX_DEVICE_QUERY_LENGTH);
  assert.equal(serializeDeviceFilters({ query, status: "all", company: "" }), `?q=${"x".repeat(MAX_DEVICE_QUERY_LENGTH)}`);
});

test("serialization omits defaults and encodes reserved characters", () => {
  assert.equal(serializeDeviceFilters(DEFAULT_DEVICE_FILTERS), "");
  assert.equal(serializeDeviceFilters({ query: "", status: "online", company: "" }), "?status=online");
  assert.equal(serializeDeviceFilters({ query: "alpha", status: "all", company: "" }), "?q=alpha");
  const search = serializeDeviceFilters({ query: "a&b=c #?/+%", status: "offline", company: "" });
  assert.equal(search, "?q=a%26b%3Dc+%23%3F%2F%2B%25&status=offline");
  assert.deepEqual(parseDeviceFilters(new URLSearchParams(search)), { query: "a&b=c #?/+%", status: "offline", company: "" });
});

test("filters by case-insensitive name or ID and by connection status", () => {
  const agents = [
    { id: "dev-001", name: "Front Desk", platform: "windows", connected: true },
    { id: "dev-002", name: "Warehouse PC", platform: "windows", connected: false },
    { id: "LAB-9", name: "Lab", platform: "windows", connected: true },
  ];
  const ids = (filters) => filterAgents(agents, filters).map((agent) => agent.id);
  assert.deepEqual(ids(DEFAULT_DEVICE_FILTERS), ["dev-001", "dev-002", "LAB-9"]);
  assert.deepEqual(ids({ query: "front DESK", status: "all", company: "" }), ["dev-001"]);
  assert.deepEqual(ids({ query: "lab-9", status: "all", company: "" }), ["LAB-9"]);
  assert.deepEqual(ids({ query: "  DEV-00 ", status: "all", company: "" }), ["dev-001", "dev-002"]);
  assert.deepEqual(ids({ query: "", status: "online", company: "" }), ["dev-001", "LAB-9"]);
  assert.deepEqual(ids({ query: "dev", status: "offline", company: "" }), ["dev-002"]);
  assert.deepEqual(ids({ query: "nothing", status: "all", company: "" }), []);
});

test("searches the operating system, model, serial number and addresses too", () => {
  const system = (os, model, serial_number, ip_addresses, public_ip = null) => ({ os, manufacturer: "Dell Inc.", model, serial_number, ip_addresses, public_ip, received_at: 1 });
  const agents = [
    { id: "w", name: "Desk", platform: "windows", connected: true, system: system("Windows 11 Pro 24H2", "OptiPlex 7090", "ABC123", ["10.1.2.3"], "203.0.113.9") },
    { id: "l", name: "Server", platform: "linux", connected: true, system: system("Ubuntu 24.04.3 LTS", "Virtual Machine", null, ["192.168.7.20", "2001:db8::20"]) },
    { id: "n", name: "New", platform: "macos", connected: false, system: null },
  ];
  const ids = (query) => filterAgents(agents, { ...DEFAULT_DEVICE_FILTERS, query }).map((agent) => agent.id);
  assert.deepEqual(ids("windows 11"), ["w"]);
  assert.deepEqual(ids("ubuntu"), ["l"]);
  assert.deepEqual(ids("optiplex"), ["w"]);
  assert.deepEqual(ids("dell"), ["w", "l"]);
  assert.deepEqual(ids("abc1"), ["w"]);
  assert.deepEqual(ids("192.168.7"), ["l"]);
  assert.deepEqual(ids("2001:DB8"), ["l"]);
  assert.deepEqual(ids("203.0.113"), ["w"]);
  assert.deepEqual(ids("new"), ["n"]);
});

test("filters by company, or by having none", () => {
  const agents = [
    { id: "a", name: "A", platform: "windows", connected: true, company_id: "acme" },
    { id: "b", name: "B", platform: "windows", connected: false, company_id: null },
    { id: "c", name: "C", platform: "windows", connected: true },
  ];
  const ids = (company) => filterAgents(agents, { ...DEFAULT_DEVICE_FILTERS, company }).map((agent) => agent.id);
  assert.deepEqual(ids(""), ["a", "b", "c"]);
  assert.deepEqual(ids("acme"), ["a"]);
  assert.deepEqual(ids(NO_COMPANY), ["b", "c"]);
  assert.deepEqual(ids("other"), []);
  const search = serializeDeviceFilters({ query: "", status: "online", company: "acme" });
  assert.equal(search, "?status=online&company=acme");
  assert.deepEqual(parseDeviceFilters(new URLSearchParams(search)), { query: "", status: "online", company: "acme" });
  assert.equal(parseDeviceFilters(new URLSearchParams({ company: "x".repeat(65) })).company, "");
});
