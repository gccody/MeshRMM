// Deployment tokens, which enroll any number of computers into one company,
// and how the website words them. Imports carry the .ts extension so
// node:test can load this module directly.
import type { AgentInstallerBootstrap } from "../enrollment/installer.ts";
import { formatRelative } from "../../lib/format.ts";

export type DeploymentTokenState = "active" | "expired" | "revoked" | "exhausted";

// A token as GET /v1/deployment-tokens lists it; its secret is never sent again.
export type DeploymentToken = {
  id: string;
  name: string;
  company_id: string;
  company_name: string;
  // email is null once the user is deleted.
  created_by: { id: string; email: string | null };
  created_at: number;
  expires_at: number;
  // null: no limit.
  max_uses: number | null;
  uses: number;
  last_used_at: number | null;
  revoked_at: number | null;
  state: DeploymentTokenState;
};

// POST /v1/deployment-tokens
export type CreatedDeploymentToken = {
  token: DeploymentToken;
  // What the installers take, with the token as its install_token.
  bootstrap: AgentInstallerBootstrap;
};

export const MAX_NAME_LENGTH = 80;
export const MAX_USES = 1_000_000;
export const DEFAULT_LIFETIME_DAYS = 30;
export const LIFETIME_CHOICES: readonly number[] = [1, 7, 30, 90, 180, 365];

export const STATE_LABELS: Record<DeploymentTokenState, string> = {
  active: "Active",
  expired: "Expired",
  revoked: "Revoked",
  exhausted: "Used up",
};

// Whether revoking would still change anything: an expired or revoked token
// enrolls nothing already.
export function canRevoke(token: Pick<DeploymentToken, "state">) {
  return token.state === "active" || token.state === "exhausted";
}

// The use limit typed into the form: null for none, undefined if invalid.
export function parseLimit(text: string): number | null | undefined {
  const trimmed = text.trim();
  if (trimmed === "") return null;
  if (!/^\d+$/.test(trimmed)) return undefined;
  const limit = Number(trimmed);
  return limit >= 1 && limit <= MAX_USES ? limit : undefined;
}

// Why the new-token form can't be sent yet, or null.
export function newTokenProblem(name: string, companyId: string, limit: string) {
  const trimmed = name.trim();
  if (!trimmed) return "Name the token, such as the rollout it's for.";
  if ([...trimmed].length > MAX_NAME_LENGTH) return `Keep the name to ${MAX_NAME_LENGTH} characters.`;
  if (!companyId) return "Choose the company its devices join.";
  if (parseLimit(limit) === undefined) return `The limit is a whole number from 1 to ${MAX_USES.toLocaleString("en-US")}, or empty for none.`;
  return null;
}

// "No devices yet", "1 device · no limit" or "3 of 10 devices".
export function usesText(token: Pick<DeploymentToken, "uses" | "max_uses">) {
  const devices = (count: number) => `${count} ${count === 1 ? "device" : "devices"}`;
  if (token.max_uses !== null) return `${token.uses} of ${devices(token.max_uses)}`;
  return token.uses === 0 ? "No devices yet · no limit" : `${devices(token.uses)} · no limit`;
}

// "Expires in 3 days", "Expired 2 hours ago", or when it was revoked.
export function expiryText(token: Pick<DeploymentToken, "expires_at" | "revoked_at">, now = Date.now()) {
  if (token.revoked_at !== null) return `Revoked ${formatRelative(token.revoked_at, now)}`;
  return `${now >= token.expires_at ? "Expired" : "Expires"} ${formatRelative(token.expires_at, now)}`;
}

export function lastUsedText(token: Pick<DeploymentToken, "last_used_at">, now = Date.now()) {
  return token.last_used_at === null ? "Never used" : `Last enrolled a device ${formatRelative(token.last_used_at, now)}`;
}

// A token after it was revoked, until the list loads again.
export function revoked(token: DeploymentToken, now = Date.now()): DeploymentToken {
  return { ...token, state: "revoked", revoked_at: token.revoked_at ?? now };
}
