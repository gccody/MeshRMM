import assert from "node:assert/strict";
import test from "node:test";
import { DeviceStore } from "../features/agents/device-store.ts";
import {
  companyName,
  matchesCompanyName,
  parseAgentEvent,
  parseAgentList,
} from "../features/agents/model.ts";
import {
  DEFAULT_IDLE_TIMEOUT_MINUTES,
  formatIdleTimeout,
  hasIdleTimeoutElapsed,
  timeoutMilliseconds,
} from "../features/session/idle-session.ts";

test("sorts and applies live Agent events deterministically", () => {
  const devices = new DeviceStore({ schedule: () => {} });
  devices.replace({
    agents: [
      { id: "b", name: "Zulu", platform: "windows", connected: false },
      { id: "a", name: "Alpha", platform: "windows", connected: true },
    ],
    companies: [],
    tags: [],
  });
  assert.deepEqual(devices.agents().map((agent) => agent.id), ["a", "b"]);

  devices.apply({
    type: "agent_upsert",
    revision: 2,
    agent: { id: "b", name: "Zulu", platform: "windows", connected: true },
  });
  assert.deepEqual(devices.agents().map((agent) => [agent.id, agent.connected]), [
    ["a", true],
    ["b", true],
  ]);

  devices.apply({ type: "agent_deleted", revision: 3, agent_id: "a" });
  assert.deepEqual(devices.agents(), [{ id: "b", name: "Zulu", platform: "windows", connected: true }]);
});

test("rejects malformed Agent API and event payloads", () => {
  assert.equal(parseAgentList({ agents: [{ id: "a" }], revision: 1 }), null);
  assert.equal(parseAgentEvent({ type: "agent_deleted", revision: -1, agent_id: "a" }), null);
  assert.deepEqual(
    parseAgentList({
      agents: [{ id: "a", name: "Alpha", platform: "windows", connected: true }],
      revision: 4,
    }),
    { agents: [{ id: "a", name: "Alpha", platform: "windows", connected: true }], companies: [], tags: [], revision: 4 },
  );
  assert.equal(parseAgentList({ agents: [], companies: [{ id: "c" }], revision: 1 }), null);
  assert.equal(parseAgentList({ agents: [], tags: [{ id: "t", name: "Servers", color: "fuchsia" }], revision: 1 }), null);
  assert.equal(parseAgentList({ agents: [{ id: "a", name: "Alpha", platform: "windows", connected: true, tag_ids: [1] }], revision: 1 }), null);
  assert.equal(parseAgentEvent({ type: "tag_upsert", revision: 5, tag: { id: "t", name: "Servers" } }), null);
  assert.equal(parseAgentEvent({ type: "tag_deleted", revision: 5 }), null);
  for (const platform of [undefined, "windows-x64", "freebsd"]) {
    assert.equal(parseAgentList({ agents: [{ id: "a", name: "Alpha", platform, connected: true }], revision: 1 }), null, String(platform));
  }
  assert.equal(
    parseAgentEvent({ type: "agent_upsert", revision: 5, agent: { id: "a", name: "A", platform: "windows", connected: true, company_id: 7 } }),
    null,
  );
  assert.equal(parseAgentEvent({ type: "company_upsert", revision: 5, company: { id: "c" } }), null);
  assert.equal(parseAgentEvent({ type: "company_deleted", revision: 5 }), null);
  assert.equal(parseAgentEvent({ type: "company_renamed", revision: 5 }), null);
  const updating = { id: "a", name: "Alpha", platform: "windows", connected: false, updating_to: "0.3.1" };
  assert.deepEqual(parseAgentEvent({ type: "agent_upsert", revision: 5, agent: updating }), {
    type: "agent_upsert",
    revision: 5,
    agent: updating,
  });
  assert.equal(
    parseAgentEvent({ type: "agent_upsert", revision: 5, agent: { ...updating, updating_to: 3 } }),
    null,
  );
});

test("accepts a device's system summary and rejects a malformed one", () => {
  const system = { os: "Ubuntu 24.04", manufacturer: null, model: "Virtual Machine", serial_number: null, ip_addresses: ["10.0.0.5"], public_ip: "203.0.113.4", received_at: 1 };
  const agent = { id: "a", name: "Alpha", platform: "linux", connected: true, system };
  assert.deepEqual(parseAgentEvent({ type: "agent_upsert", revision: 5, agent }).agent.system, system);
  assert.ok(parseAgentEvent({ type: "agent_upsert", revision: 5, agent: { ...agent, system: null } }));
  for (const broken of [{ ...system, os: 7 }, { ...system, ip_addresses: "10.0.0.5" }, { ...system, ip_addresses: [5] }, { ...system, received_at: "now" }, "Ubuntu"]) {
    assert.equal(parseAgentEvent({ type: "agent_upsert", revision: 5, agent: { ...agent, system: broken } }), null, JSON.stringify(broken));
  }
});

test("applies company events and names a device's company", () => {
  const devices = new DeviceStore({ schedule: () => {} });
  devices.replace({ agents: [{ id: "a", name: "Alpha", platform: "windows", connected: true, company_id: "g" }], companies: [], tags: [] });
  const before = devices.agents();
  devices.apply({ type: "company_upsert", revision: 1, company: { id: "g", name: "globex" } });
  devices.apply({ type: "company_upsert", revision: 2, company: { id: "a", name: "Acme" } });
  assert.deepEqual(devices.companies().map((company) => company.name), ["Acme", "globex"]);
  assert.equal(companyName(devices.companies(), devices.agents()[0].company_id), "globex");

  devices.apply({ type: "company_upsert", revision: 3, company: { id: "g", name: "Globex" } });
  assert.equal(companyName(devices.companies(), "g"), "Globex");
  devices.apply({ type: "company_deleted", revision: 4, company_id: "g" });
  assert.deepEqual(devices.companies(), [{ id: "a", name: "Acme" }]);
  assert.equal(companyName(devices.companies(), "g"), null);
  assert.equal(companyName(devices.companies(), null), null);
  assert.equal(devices.agents(), before, "company events leave devices alone");
});

test("matches company names to a search, ignoring case and spaces around it", () => {
  assert.equal(matchesCompanyName("Globex Corporation", "  globex "), true);
  assert.equal(matchesCompanyName("Globex Corporation", "CORP"), true);
  assert.equal(matchesCompanyName("Globex Corporation", "acme"), false);
  assert.equal(matchesCompanyName("Globex Corporation", ""), true);
});

test("uses a four-hour idle timeout by default", () => {
  assert.equal(DEFAULT_IDLE_TIMEOUT_MINUTES, 240);
  assert.equal(timeoutMilliseconds(DEFAULT_IDLE_TIMEOUT_MINUTES), 14_400_000);
  assert.equal(formatIdleTimeout(DEFAULT_IDLE_TIMEOUT_MINUTES), "4 hours");
  assert.equal(hasIdleTimeoutElapsed(1_000, 240, 14_400_999), false);
  assert.equal(hasIdleTimeoutElapsed(1_000, 240, 14_401_000), true);
});

test("falls back to the safe idle default for an invalid policy", () => {
  assert.equal(timeoutMilliseconds(0), 14_400_000);
  assert.equal(timeoutMilliseconds(1_441), 14_400_000);
});
