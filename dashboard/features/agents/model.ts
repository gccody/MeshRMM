import { type Agent, type AgentEvent, type Company, type Inventory, PLATFORM_LABELS, type SystemSummary, TAG_COLORS, type Tag } from "./types.ts";

const PLATFORMS: string[] = Object.keys(PLATFORM_LABELS);

// Built once: localeCompare with options builds a collator each call.
const NAMES = new Intl.Collator();
const NAMES_IGNORING_CASE = new Intl.Collator(undefined, { sensitivity: "base" });

// The order the device list shows: online devices first, then by name, then
// by ID. The server sorts by lowercased name, which the browser's collation
// can order differently, so the browser sorts what it receives.
export const compareAgents = (left: Agent, right: Agent) =>
  Number(right.connected) - Number(left.connected) ||
  NAMES.compare(left.name, right.name) ||
  NAMES.compare(left.id, right.id);

// The chosen devices still in the inventory, in the list's order: found by
// ID rather than by going through every device.
export function chosenAgents(ids: Iterable<string>, devices: { get(id: string): Agent | undefined }) {
  const chosen: Agent[] = [];
  for (const id of ids) {
    const agent = devices.get(id);
    if (agent) chosen.push(agent);
  }
  return chosen.sort(compareAgents);
}

// Companies and tags: by name ignoring case, then by ID.
export const compareByName = (left: Company | Tag, right: Company | Tag) =>
  NAMES_IGNORING_CASE.compare(left.name, right.name) || NAMES.compare(left.id, right.id);

const isText = (value: unknown) => value === null || value === undefined || typeof value === "string";

const isSystemSummary = (value: unknown): value is SystemSummary | null | undefined => {
  if (value === null || value === undefined) return true;
  if (typeof value !== "object") return false;
  const candidate = value as Partial<SystemSummary>;
  return (
    typeof candidate.os === "string" &&
    isText(candidate.manufacturer) &&
    isText(candidate.model) &&
    isText(candidate.serial_number) &&
    isText(candidate.public_ip) &&
    Array.isArray(candidate.ip_addresses) &&
    candidate.ip_addresses.every((address) => typeof address === "string") &&
    typeof candidate.received_at === "number"
  );
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
    (candidate.company_id === undefined || candidate.company_id === null || typeof candidate.company_id === "string") &&
    (candidate.tag_ids === undefined || (Array.isArray(candidate.tag_ids) && candidate.tag_ids.every((id) => typeof id === "string"))) &&
    (candidate.notes_revision === undefined || Number.isSafeInteger(candidate.notes_revision)) &&
    isSystemSummary(candidate.system)
  );
};

const isCompany = (value: unknown): value is Company => {
  if (!value || typeof value !== "object") return false;
  const candidate = value as Partial<Company>;
  return typeof candidate.id === "string" && typeof candidate.name === "string";
};

const COLORS: readonly string[] = TAG_COLORS;

const isTag = (value: unknown): value is Tag => {
  if (!value || typeof value !== "object") return false;
  const candidate = value as Partial<Tag>;
  return typeof candidate.id === "string" && typeof candidate.name === "string" && COLORS.includes(candidate.color as string);
};

// A snapshot's companies, or null when they're malformed.
const parseCompanies = (value: unknown): Company[] | null =>
  value === undefined ? [] : Array.isArray(value) && value.every(isCompany) ? value : null;

// A snapshot's tags, or null when they're malformed.
const parseTags = (value: unknown): Tag[] | null =>
  value === undefined ? [] : Array.isArray(value) && value.every(isTag) ? value : null;

export const parseAgentList = (
  value: unknown,
): (Inventory & { revision: number }) | null => {
  if (!value || typeof value !== "object") return null;
  const candidate = value as { agents?: unknown; companies?: unknown; tags?: unknown; revision?: unknown };
  if (!Array.isArray(candidate.agents) || !candidate.agents.every(isAgent)) {
    return null;
  }
  const companies = parseCompanies(candidate.companies);
  const tags = parseTags(candidate.tags);
  if (!companies || !tags) return null;
  const revision = Number.isSafeInteger(candidate.revision)
    ? (candidate.revision as number)
    : -1;
  return { agents: candidate.agents, companies, tags, revision };
};

export const parseAgentEvent = (value: unknown): AgentEvent | null => {
  if (!value || typeof value !== "object") return null;
  const candidate = value as Record<string, unknown>;
  if (!Number.isSafeInteger(candidate.revision) || (candidate.revision as number) < 0) {
    return null;
  }
  switch (candidate.type) {
    case "snapshot":
      return Array.isArray(candidate.agents) && candidate.agents.every(isAgent) && parseCompanies(candidate.companies) && parseTags(candidate.tags)
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
    case "tag_upsert":
      return isTag(candidate.tag) ? candidate as AgentEvent : null;
    case "tag_deleted":
      return typeof candidate.tag_id === "string" ? candidate as AgentEvent : null;
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
