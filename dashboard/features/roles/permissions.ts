// Permissions grouped by what they govern, for the role editor. Imports carry
// the .ts extension so node:test can load this module directly.
import type { PermissionInfo } from "../users/model.ts";

const GROUP_LABELS: Record<string, string> = {
  devices: "Devices",
  companies: "Companies",
  tags: "Tags",
  custom_fields: "Custom fields",
  sessions: "Remote sessions",
  scripts: "Scripts",
  files: "Files",
  updates: "Updates",
  jobs: "Jobs",
  automation: "Automation",
  alerts: "Alerts",
  users: "Users",
  roles: "Roles",
  settings: "Settings",
  authentication: "Authentication",
  api_tokens: "API tokens",
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

// Whether a role limited to some companies can grant the permission: one
// that acts on what the whole server shares (users, roles, settings, the
// shared toolbox, the application library, policies, alert rules, companies
// and tags themselves, the audit log) needs a role for every company.
export const grantableOnSomeCompanies = (permission: Pick<PermissionInfo, "reach">) => permission.reach !== "instance";

// What a role saves of the permissions ticked: on a role limited to some
// companies, only those such a role can grant.
export function savedPermissions(granted: Iterable<PermissionInfo["name"]>, permissions: readonly PermissionInfo[], allCompanies: boolean) {
  const ticked = new Set(granted);
  return permissions
    .filter((permission) => ticked.has(permission.name) && (allCompanies || grantableOnSomeCompanies(permission)))
    .map((permission) => permission.name);
}

// The companies a role reaches, in words: "Every company", "Acme",
// "Acme and Globex", or "3 companies".
export function companyScopeLabel(role: { all_companies: boolean; company_ids: readonly string[] }, names: ReadonlyMap<string, string>) {
  if (role.all_companies) return "Every company";
  const named = role.company_ids.map((id) => names.get(id)).filter((name): name is string => Boolean(name));
  if (!role.company_ids.length) return "No companies";
  if (named.length === role.company_ids.length && named.length <= 2) return named.join(" and ");
  const count = role.company_ids.length;
  return `${count} ${count === 1 ? "company" : "companies"}`;
}
