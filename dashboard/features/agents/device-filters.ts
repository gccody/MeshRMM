import { type TagMatch, hasTags, tagsOf } from "../tags/model.ts";
import type { Agent, Tag } from "./types.ts";

// The Devices filters live in the URL (?q=…&status=…&company=…&tags=…) so a
// reload, a shared link, or the Devices navigation entry restores them.
export type AgentStatusFilter = "all" | "online" | "offline";

// A company's ID, NO_COMPANY for devices in none, or "" for every device.
export type CompanyFilter = string;
export const NO_COMPANY = "none";

// Tags' IDs (devices with any, or all, of them), or none for every device.
export type DeviceFilters = { query: string; status: AgentStatusFilter; company: CompanyFilter; tags: string[]; tagMatch: TagMatch };

export const DEFAULT_DEVICE_FILTERS: DeviceFilters = { query: "", status: "all", company: "", tags: [], tagMatch: "any" };
export const MAX_DEVICE_QUERY_LENGTH = 200;
const MAX_COMPANY_LENGTH = 64;
const MAX_TAG_ID_LENGTH = 64;
export const MAX_FILTER_TAGS = 20;

const STATUSES: readonly AgentStatusFilter[] = ["all", "online", "offline"];

export function isAgentStatusFilter(value: unknown): value is AgentStatusFilter {
  return STATUSES.includes(value as AgentStatusFilter);
}

export function clampDeviceQuery(query: string) {
  return query.slice(0, MAX_DEVICE_QUERY_LENGTH);
}

export function parseDeviceFilters(params: { get(name: string): string | null } | null | undefined): DeviceFilters {
  const status = params?.get("status");
  const company = params?.get("company") ?? "";
  const tags = (params?.get("tags") ?? "").split(",").filter((id) => id && id.length <= MAX_TAG_ID_LENGTH);
  return {
    query: clampDeviceQuery(params?.get("q") ?? ""),
    status: isAgentStatusFilter(status) ? status : "all",
    company: company.length <= MAX_COMPANY_LENGTH ? company : "",
    tags: [...new Set(tags)].slice(0, MAX_FILTER_TAGS),
    tagMatch: params?.get("tag_match") === "all" ? "all" : "any",
  };
}

// Returns "" for the defaults, otherwise a "?…" search string.
export function serializeDeviceFilters({ query, status, company, tags, tagMatch }: DeviceFilters) {
  const params = new URLSearchParams();
  if (query) params.set("q", clampDeviceQuery(query));
  if (status !== "all") params.set("status", status);
  if (company) params.set("company", company);
  if (tags.length) {
    params.set("tags", tags.slice(0, MAX_FILTER_TAGS).join(","));
    if (tagMatch === "all") params.set("tag_match", "all");
  }
  const search = params.toString();
  return search ? `?${search}` : "";
}

// What the search box matches: the device's name, ID and tags' names, and
// from its system information its operating system, manufacturer, model,
// serial number and IP addresses.
export function searchText(agent: Pick<Agent, "id" | "name" | "system" | "tag_ids">, tags: readonly Tag[] = []) {
  const system = agent.system;
  const parts = [agent.name, agent.id, ...tagsOf(agent.tag_ids, tags).map((tag) => tag.name)];
  if (system) {
    parts.push(system.os, system.manufacturer ?? "", system.model ?? "", system.serial_number ?? "", system.public_ip ?? "", ...system.ip_addresses);
  }
  return parts.join("\n").toLowerCase();
}

// `tags` are the inventory's, whose names the search matches.
export function filterAgents<T extends Pick<Agent, "id" | "name" | "connected" | "company_id" | "system" | "tag_ids">>(agents: T[], filters: DeviceFilters, tags: readonly Tag[] = []) {
  const { query, status, company, tagMatch } = filters;
  const search = query.trim().toLowerCase();
  return agents.filter((agent) => {
    const matchesSearch = !search || searchText(agent, tags).includes(search);
    const matchesStatus = status === "all" || (status === "online" ? agent.connected : !agent.connected);
    const matchesCompany = !company || (company === NO_COMPANY ? !agent.company_id : agent.company_id === company);
    return matchesSearch && matchesStatus && matchesCompany && hasTags(agent, filters.tags, tagMatch);
  });
}
