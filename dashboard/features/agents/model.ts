import { type Agent, type AgentDelta, type AgentEvent, type Company, type Inventory, PLATFORM_LABELS } from "./types.ts";

const PLATFORMS: string[] = Object.keys(PLATFORM_LABELS);

export const EMPTY_INVENTORY: Inventory = { agents: [], companies: [] };

export const sortAgents = (items: Agent[]) =>
  [...items].sort(
    (left, right) =>
      Number(right.connected) - Number(left.connected) ||
      left.name.localeCompare(right.name) ||
      left.id.localeCompare(right.id),
  );

export const sortCompanies = (items: Company[]) =>
  [...items].sort(
    (left, right) =>
      left.name.localeCompare(right.name, undefined, { sensitivity: "base" }) ||
      left.id.localeCompare(right.id),
  );

export const applyAgentDelta = (items: Agent[], event: Extract<AgentDelta, { type: "agent_upsert" | "agent_deleted" }>) =>
  event.type === "agent_upsert"
    ? sortAgents([
        ...items.filter((agent) => agent.id !== event.agent.id),
        event.agent,
      ])
    : items.filter((agent) => agent.id !== event.agent_id);

export const applyInventoryDelta = (inventory: Inventory, event: AgentDelta): Inventory => {
  switch (event.type) {
    case "agent_upsert":
    case "agent_deleted":
      return { ...inventory, agents: applyAgentDelta(inventory.agents, event) };
    case "company_upsert":
      return {
        ...inventory,
        companies: sortCompanies([
          ...inventory.companies.filter((company) => company.id !== event.company.id),
          event.company,
        ]),
      };
    case "company_deleted":
      return { ...inventory, companies: inventory.companies.filter((company) => company.id !== event.company_id) };
  }
};

const isAgent = (value: unknown): value is Agent => {
  if (!value || typeof value !== "object") return false;
  const candidate = value as Partial<Agent>;
  return (
    typeof candidate.id === "string" &&
    typeof candidate.name === "string" &&
    PLATFORMS.includes(candidate.platform as string) &&
    typeof candidate.connected === "boolean" &&
    (candidate.updating_to === undefined || typeof candidate.updating_to === "string") &&
    (candidate.company_id === undefined || candidate.company_id === null || typeof candidate.company_id === "string")
  );
};

const isCompany = (value: unknown): value is Company => {
  if (!value || typeof value !== "object") return false;
  const candidate = value as Partial<Company>;
  return typeof candidate.id === "string" && typeof candidate.name === "string";
};

// A snapshot's companies, or null when they're malformed.
const parseCompanies = (value: unknown): Company[] | null =>
  value === undefined ? [] : Array.isArray(value) && value.every(isCompany) ? value : null;

export const parseAgentList = (
  value: unknown,
): (Inventory & { revision: number }) | null => {
  if (!value || typeof value !== "object") return null;
  const candidate = value as { agents?: unknown; companies?: unknown; revision?: unknown };
  if (!Array.isArray(candidate.agents) || !candidate.agents.every(isAgent)) {
    return null;
  }
  const companies = parseCompanies(candidate.companies);
  if (!companies) return null;
  const revision = Number.isSafeInteger(candidate.revision)
    ? (candidate.revision as number)
    : -1;
  return { agents: candidate.agents, companies, revision };
};

export const parseAgentEvent = (value: unknown): AgentEvent | null => {
  if (!value || typeof value !== "object") return null;
  const candidate = value as Record<string, unknown>;
  if (!Number.isSafeInteger(candidate.revision) || (candidate.revision as number) < 0) {
    return null;
  }
  switch (candidate.type) {
    case "snapshot":
      return Array.isArray(candidate.agents) && candidate.agents.every(isAgent) && parseCompanies(candidate.companies)
        ? candidate as AgentEvent
        : null;
    case "agent_upsert":
      return isAgent(candidate.agent) ? candidate as AgentEvent : null;
    case "agent_deleted":
      return typeof candidate.agent_id === "string" ? candidate as AgentEvent : null;
    case "company_upsert":
      return isCompany(candidate.company) ? candidate as AgentEvent : null;
    case "company_deleted":
      return typeof candidate.company_id === "string" ? candidate as AgentEvent : null;
    default:
      return null;
  }
};

// The company's name, or null for a device in none (or one this browser
// hasn't heard of yet).
export const companyName = (companies: readonly Company[], id: string | null | undefined) =>
  (id && companies.find((company) => company.id === id)?.name) || null;

// Whether a company's name matches a search box, ignoring case.
export const matchesCompanyName = (name: string, query: string) =>
  name.toLocaleLowerCase().includes(query.trim().toLocaleLowerCase());
