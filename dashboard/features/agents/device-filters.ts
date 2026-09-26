import type { Agent } from "./types";

// The Devices filters live in the URL (?q=…&status=…) so a reload, a shared
// link, or the Devices navigation entry restores them.
export type AgentStatusFilter = "all" | "online" | "offline";

export type DeviceFilters = { query: string; status: AgentStatusFilter };

export const DEFAULT_DEVICE_FILTERS: DeviceFilters = { query: "", status: "all" };
export const MAX_DEVICE_QUERY_LENGTH = 200;

const STATUSES: readonly AgentStatusFilter[] = ["all", "online", "offline"];

export function isAgentStatusFilter(value: unknown): value is AgentStatusFilter {
  return STATUSES.includes(value as AgentStatusFilter);
}

export function clampDeviceQuery(query: string) {
  return query.slice(0, MAX_DEVICE_QUERY_LENGTH);
}

export function parseDeviceFilters(params: { get(name: string): string | null } | null | undefined): DeviceFilters {
  const status = params?.get("status");
  return {
    query: clampDeviceQuery(params?.get("q") ?? ""),
    status: isAgentStatusFilter(status) ? status : "all",
  };
}

// Returns "" for the defaults, otherwise a "?…" search string.
export function serializeDeviceFilters({ query, status }: DeviceFilters) {
  const params = new URLSearchParams();
  if (query) params.set("q", clampDeviceQuery(query));
  if (status !== "all") params.set("status", status);
  const search = params.toString();
  return search ? `?${search}` : "";
}

export function filterAgents<T extends Pick<Agent, "id" | "name" | "connected">>(agents: T[], { query, status }: DeviceFilters) {
  const search = query.trim().toLowerCase();
  return agents.filter((agent) => {
    const matchesSearch = !search || `${agent.name} ${agent.id}`.toLowerCase().includes(search);
    const matchesStatus = status === "all" || (status === "online" ? agent.connected : !agent.connected);
    return matchesSearch && matchesStatus;
  });
}
