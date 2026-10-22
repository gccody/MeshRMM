// Personal API tokens and how the website words them. Imports carry the .ts
// extension so node:test can load this module directly.
import type { Permission } from "../auth/types.ts";
import type { PermissionInfo } from "../users/model.ts";
import { formatRelative } from "../../lib/format.ts";

// A token as GET /v1/api-tokens lists it; the secret is never sent again.
export type ApiToken = {
  id: string;
  name: string;
  user: { id: string; email: string; display_name: string };
  // null: all of its user's permissions.
  permissions: Permission[] | null;
  created_at: number;
  // null: never expires.
  expires_at: number | null;
  last_used_at: number | null;
  last_used_ip: string | null;
};

export type ApiTokenPolicy = {
  enabled: boolean;
  // null: no limit, so tokens may never expire.
  max_lifetime_days: number | null;
};

// GET /v1/api-tokens
export type ApiTokenList = {
  tokens: ApiToken[];
  policy: ApiTokenPolicy;
  // What the signed-in user may limit a token to: their own permissions.
  permissions: PermissionInfo[];
};

// POST /v1/api-tokens
export type CreatedApiToken = ApiToken & { secret: string };

const LIFETIMES = [7, 30, 90, 365];

// The lifetimes to offer under the server's cap: the usual ones that fit,
// the cap itself when it isn't one of them, and "never" without a cap.
export function lifetimeChoices(maxDays: number | null): { days: number | null; label: string }[] {
  const days = LIFETIMES.filter((value) => maxDays === null || value <= maxDays);
  if (maxDays !== null && !days.includes(maxDays)) days.push(maxDays);
  const choices: { days: number | null; label: string }[] = days.map((value) => ({ days: value, label: value === 1 ? "1 day" : `${value} days` }));
  if (maxDays === null) choices.push({ days: null, label: "Never" });
  return choices;
}

// The default choice: 30 days, or the longest allowed when that's less.
export function defaultLifetime(maxDays: number | null) {
  return maxDays === null ? 30 : Math.min(30, maxDays);
}

export function isExpired(token: Pick<ApiToken, "expires_at">, now = Date.now()) {
  return token.expires_at !== null && now >= token.expires_at;
}

// "Expires in 3 days", "Expired 2 hours ago" or "Never expires".
export function expiryText(token: Pick<ApiToken, "expires_at">, now = Date.now()) {
  if (token.expires_at === null) return "Never expires";
  return `${isExpired(token, now) ? "Expired" : "Expires"} ${formatRelative(token.expires_at, now)}`;
}

// "Never used" or "Used 5 minutes ago from 203.0.113.7".
export function lastUsedText(token: Pick<ApiToken, "last_used_at" | "last_used_ip">, now = Date.now()) {
  if (token.last_used_at === null) return "Never used";
  const when = `Used ${formatRelative(token.last_used_at, now)}`;
  return token.last_used_ip ? `${when} from ${token.last_used_ip}` : when;
}

// What a token may do, in a few words.
export function scopeText(token: Pick<ApiToken, "permissions">) {
  if (token.permissions === null) return "All of its user's permissions";
  if (token.permissions.length <= 3) return token.permissions.join(", ");
  return `${token.permissions.length} permissions`;
}

// Why the new-token form can't be sent yet, or null.
export function newTokenProblem(name: string, scoped: boolean, chosen: ReadonlySet<string>) {
  if (!name.trim()) return "Name the token after what will use it.";
  if (scoped && chosen.size === 0) return "Choose at least one permission, or allow all of yours.";
  return null;
}
