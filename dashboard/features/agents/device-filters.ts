import type { Agent } from "./types";

// The Devices filters live in the URL (?q=…&status=…&company=…) so a reload,
// a shared link, or the Devices navigation entry restores them.
export type AgentStatusFilter = "all" | "online" | "offline";

// A company's ID, NO_COMPANY for devices in none, or "" for every device.
export type CompanyFilter = string;
export const NO_COMPANY = "none";

export type DeviceFilters = { query: string; status: AgentStatusFilter; company: CompanyFilter };

export const DEFAULT_DEVICE_FILTERS: DeviceFilters = { query: "", status: "all", company: "" };
export const MAX_DEVICE_QUERY_LENGTH = 200;
const MAX_COMPANY_LENGTH = 64;

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
  return {
    query: clampDeviceQuery(params?.get("q") ?? ""),
    status: isAgentStatusFilter(status) ? status : "all",
    company: company.length <= MAX_COMPANY_LENGTH ? company : "",
  };
}

// Returns "" for the defaults, otherwise a "?…" search string.
export function serializeDeviceFilters({ query, status, company }: DeviceFilters) {
  const params = new URLSearchParams();
  if (query) params.set("q", clampDeviceQuery(query));
  if (status !== "all") params.set("status", status);
  if (company) params.set("company", company);
  const search = params.toString();
  return search ? `?${search}` : "";
}

export function filterAgents<T extends Pick<Agent, "id" | "name" | "connected" | "company_id">>(agents: T[], { query, status, company }: DeviceFilters) {
  const search = query.trim().toLowerCase();
  return agents.filter((agent) => {
    const matchesSearch = !search || `${agent.name} ${agent.id}`.toLowerCase().includes(search);
    const matchesStatus = status === "all" || (status === "online" ? agent.connected : !agent.connected);
    const matchesCompany = !company || (company === NO_COMPANY ? !agent.company_id : agent.company_id === company);
    return matchesSearch && matchesStatus && matchesCompany;
  });
}
