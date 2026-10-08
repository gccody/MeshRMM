import assert from "node:assert/strict";
import test from "node:test";
import {
  applyAgentDelta,
  parseAgentEvent,
  parseAgentList,
  sortAgents,
} from "../features/agents/model.ts";
import {
  DEFAULT_IDLE_TIMEOUT_MINUTES,
  formatIdleTimeout,
  hasIdleTimeoutElapsed,
  timeoutMilliseconds,
} from "../features/session/idle-session.ts";

test("sorts and applies live Agent events deterministically", () => {
  const agents = sortAgents([
    { id: "b", name: "Zulu", connected: false },
    { id: "a", name: "Alpha", connected: true },
  ]);
  assert.deepEqual(agents.map((agent) => agent.id), ["a", "b"]);

  const updated = applyAgentDelta(agents, {
    type: "agent_upsert",
    revision: 2,
    agent: { id: "b", name: "Zulu", connected: true },
  });
  assert.deepEqual(updated.map((agent) => [agent.id, agent.connected]), [
    ["a", true],
    ["b", true],
  ]);

  assert.deepEqual(
    applyAgentDelta(updated, { type: "agent_deleted", revision: 3, agent_id: "a" }),
    [{ id: "b", name: "Zulu", connected: true }],
  );
});

test("rejects malformed Agent API and event payloads", () => {
  assert.equal(parseAgentList({ agents: [{ id: "a" }], revision: 1 }), null);
  assert.equal(parseAgentEvent({ type: "agent_deleted", revision: -1, agent_id: "a" }), null);
  assert.deepEqual(
    parseAgentList({
      agents: [{ id: "a", name: "Alpha", connected: true }],
      revision: 4,
    }),
    { agents: [{ id: "a", name: "Alpha", connected: true }], revision: 4 },
  );
  const updating = { id: "a", name: "Alpha", connected: false, updating_to: "0.3.1" };
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
