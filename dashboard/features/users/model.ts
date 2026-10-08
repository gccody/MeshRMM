// Users, invitations and roles as the server sends them.
import type { Permission, RoleRef } from "../auth/types";

export type UserView = {
  id: string;
  email: string;
  display_name: string;
  disabled: boolean;
  has_password: boolean;
  // An authenticator app or a passkey.
  two_factor_enabled: boolean;
  passkeys: number;
  // Signed in with SSO, which linked the account to an identity there.
  sso_linked: boolean;
  // Created or changed by the identity provider through SCIM.
  scim_managed: boolean;
  // Roles assigned here.
  roles: RoleRef[];
  // Roles from identity provider groups, which only the provider changes.
  group_roles: GroupRoleRef[];
  created_at: number;
  last_sign_in_at: number | null;
};

export type GroupRoleRef = RoleRef & { source: "scim" | "sso"; group: string };

export type InvitationView = {
  id: string;
  email: string;
  roles: RoleRef[];
  created_by_user_id: string | null;
  created_at: number;
  expires_at: number;
  // Renew it for a working link.
  expired: boolean;
};

// How a one-time link reached its person: by email, or for the
// administrator to pass on.
export type Delivery = {
  emailed: boolean;
  link?: string;
  // Email is set up but sending failed.
  email_error?: string;
};

export type CreatedInvitation = Delivery & { invitation: InvitationView };

export type ResetLink = Delivery & { expires_at: number };

export type Role = {
  id: string;
  name: string;
  description: string;
  // "administrator" or "technician" for the built-in roles.
  builtin: string | null;
  permissions: Permission[];
  member_count: number;
};

export type PermissionInfo = { name: Permission; description: string };

export const ADMINISTRATOR_ROLE_ID = "administrator";

const collator = new Intl.Collator(undefined, { sensitivity: "base", numeric: true });

// Built-in roles first, then by name.
export function sortRoles<T extends { name: string; builtin: string | null }>(roles: readonly T[]) {
  return [...roles].sort((left, right) => Number(right.builtin !== null) - Number(left.builtin !== null) || collator.compare(left.name, right.name));
}

export function sortUsers<T extends { display_name: string; email: string }>(users: readonly T[]) {
  return [...users].sort((left, right) => collator.compare(left.display_name || left.email, right.display_name || right.email));
}

// Whether a user matches the search box: by name, email or role, including
// roles from groups.
export function matchesUser(user: Pick<UserView, "display_name" | "email" | "roles"> & Partial<Pick<UserView, "group_roles">>, query: string) {
  const needle = query.trim().toLocaleLowerCase();
  if (!needle) return true;
  const roles = [...user.roles, ...(user.group_roles ?? [])].map((role) => role.name);
  return [user.display_name, user.email, ...roles].some((value) => value.toLocaleLowerCase().includes(needle));
}

// "Technician · via Okta group “IT”": SSO groups name the provider when the
// server's sign-in button does.
export function groupRoleLabel(role: Pick<GroupRoleRef, "name" | "source" | "group">, ssoName?: string | null) {
  const via = role.source === "scim" ? "SCIM" : ssoName || "SSO";
  return `${role.name} · via ${via} group “${role.group}”`;
}
