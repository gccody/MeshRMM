// Permissions grouped by what they govern, for the role editor. Imports carry
// the .ts extension so node:test can load this module directly.
import type { PermissionInfo } from "../users/model.ts";

const GROUP_LABELS: Record<string, string> = {
  devices: "Devices",
  sessions: "Remote sessions",
  scripts: "Scripts",
  files: "Files",
  users: "Users",
  roles: "Roles",
  settings: "Settings",
  authentication: "Authentication",
  audit: "Audit log",
};

// Groups in the server's order, which lists related permissions together.
export function groupPermissions(permissions: readonly PermissionInfo[]) {
  const groups = new Map<string, PermissionInfo[]>();
  for (const permission of permissions) {
    const area = permission.name.split(".")[0];
    const group = groups.get(area);
    if (group) group.push(permission);
    else groups.set(area, [permission]);
  }
  return [...groups.entries()].map(([area, items]) => ({ label: GROUP_LABELS[area] ?? area, items }));
}
