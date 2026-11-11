// The remote tools: a device's processes, services and Windows registry,
// without a remote session. Imports carry the .ts extension so node:test can load this module
// directly.
import type { Permission } from "../auth/types.ts";
import type { DevicePlatform } from "../agents/types.ts";

export type Tool = "processes" | "services" | "registry";

export const TOOLS: readonly Tool[] = ["processes", "services", "registry"];

export const TOOL_LABELS: Record<Tool, string> = {
  processes: "Processes",
  services: "Services",
  registry: "Registry",
};

const VIEW_PERMISSION: Record<Tool, Permission> = {
  processes: "tools.processes.view",
  services: "tools.services.view",
  registry: "tools.registry.view",
};

const MANAGE_PERMISSION: Record<Tool, Permission> = {
  processes: "tools.processes.manage",
  services: "tools.services.manage",
  registry: "tools.registry.manage",
};

export const canViewTool = (can: (permission: Permission) => boolean, tool: Tool) => can(VIEW_PERMISSION[tool]);
export const canManageTool = (can: (permission: Permission) => boolean, tool: Tool) => can(MANAGE_PERMISSION[tool]);

// The platforms each tool works on, and why the others lack it.
const PLATFORMS: Record<Tool, { on: readonly DevicePlatform[]; otherwise: string }> = {
  processes: { on: ["windows", "macos", "linux"], otherwise: "" },
  services: { on: ["windows", "macos", "linux"], otherwise: "" },
  registry: { on: ["windows"], otherwise: "Only Windows has a registry." },
};

// Why a device's platform lacks `tool`, or null when it has it.
export function unavailableReason(tool: Tool, platform: DevicePlatform): string | null {
  const { on, otherwise } = PLATFORMS[tool];
  return on.includes(platform) ? null : otherwise;
}

export const toolsPagePath = (deviceId: string, tool: Tool = "processes") =>
  `/tools?id=${encodeURIComponent(deviceId)}&tool=${tool}`;

export const parseTool = (value: string | null): Tool | null =>
  TOOLS.find((tool) => tool === value) ?? null;
