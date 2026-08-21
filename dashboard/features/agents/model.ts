import type { Agent, AgentDelta, AgentEvent } from "./types";

export const sortAgents = (items: Agent[]) =>
  [...items].sort(
    (left, right) =>
      Number(right.connected) - Number(left.connected) ||
      left.name.localeCompare(right.name) ||
      left.id.localeCompare(right.id),
  );

export const applyAgentDelta = (items: Agent[], event: AgentDelta) =>
  event.type === "agent_upsert"
    ? sortAgents([
        ...items.filter((agent) => agent.id !== event.agent.id),
        event.agent,
      ])
    : items.filter((agent) => agent.id !== event.agent_id);

const isAgent = (value: unknown): value is Agent => {
  if (!value || typeof value !== "object") return false;
  const candidate = value as Partial<Agent>;
  return (
    typeof candidate.id === "string" &&
    typeof candidate.name === "string" &&
    typeof candidate.connected === "boolean"
  );
};

export const parseAgentList = (
  value: unknown,
): { agents: Agent[]; revision: number } | null => {
  if (!value || typeof value !== "object") return null;
  const candidate = value as { agents?: unknown; revision?: unknown };
  if (!Array.isArray(candidate.agents) || !candidate.agents.every(isAgent)) {
    return null;
  }
  const revision = Number.isSafeInteger(candidate.revision)
    ? (candidate.revision as number)
    : -1;
  return { agents: candidate.agents, revision };
};

export const parseAgentEvent = (value: unknown): AgentEvent | null => {
  if (!value || typeof value !== "object") return null;
  const candidate = value as Record<string, unknown>;
  if (!Number.isSafeInteger(candidate.revision) || (candidate.revision as number) < 0) {
    return null;
  }
  if (candidate.type === "snapshot") {
    if (!Array.isArray(candidate.agents) || !candidate.agents.every(isAgent)) return null;
    return candidate as AgentEvent;
  }
  if (candidate.type === "agent_upsert" && isAgent(candidate.agent)) {
    return candidate as AgentEvent;
  }
  if (candidate.type === "agent_deleted" && typeof candidate.agent_id === "string") {
    return candidate as AgentEvent;
  }
  return null;
};

