// Users, invitations and roles as the server sends them.
import type { Permission, RoleRef } from "../auth/types";

export type UserView = {
  id: string;
  email: string;
  display_name: string;
  disabled: boolean;
  has_password: boolean;
  two_factor_enabled: boolean;
  roles: RoleRef[];
  created_at: number;
  last_sign_in_at: number | null;
};

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

// Whether a user matches the search box: by name, email or role.
export function matchesUser(user: Pick<UserView, "display_name" | "email" | "roles">, query: string) {
  const needle = query.trim().toLocaleLowerCase();
  if (!needle) return true;
  return [user.display_name, user.email, ...user.roles.map((role) => role.name)].some((value) => value.toLocaleLowerCase().includes(needle));
}
