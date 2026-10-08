// The account's two-factor methods as the account page words them. Imports
// carry the .ts extension so node:test can load this module directly.
import type { Account } from "../auth/types.ts";

// GET /v1/account/passkeys
export type PasskeyView = {
  id: string;
  name: string;
  created_at: number;
  last_used_at: number | null;
};

// POST /v1/account/passkeys. Recovery codes come when the passkey turned
// two-factor authentication on; they are shown once.
export type AddedPasskey = { passkey: PasskeyView; recovery_codes: string[] | null };

export const MAX_PASSKEY_NAME_LENGTH = 120;

const count = (number: number, one: string, many: string) => (number === 1 ? `1 ${one}` : `${number} ${many}`);

// "On, with an authenticator app and 2 passkeys."
export function twoFactorSummary(twoFactor: Pick<Account["two_factor"], "enabled" | "totp" | "passkeys">) {
  if (!twoFactor.enabled) return "Off. Signing in takes only your password.";
  const methods = [
    ...(twoFactor.totp ? ["an authenticator app"] : []),
    ...(twoFactor.passkeys > 0 ? [count(twoFactor.passkeys, "passkey", "passkeys")] : []),
  ];
  return methods.length ? `On, with ${methods.join(" and ")}.` : "On.";
}

// A name for a new passkey, from the browser it's made in: "Mac passkey".
// People rename it when it lives on a phone or security key instead.
export function defaultPasskeyName(userAgent: string | null | undefined) {
  const agent = userAgent ?? "";
  const device = /iPhone/.test(agent) ? "iPhone"
    : /iPad/.test(agent) ? "iPad"
    : /Android/.test(agent) ? "Android"
    : /Windows/.test(agent) ? "Windows"
    : /Mac OS X|Macintosh/.test(agent) ? "Mac"
    : /CrOS/.test(agent) ? "Chromebook"
    : /Linux/.test(agent) ? "Linux"
    : null;
  return device ? `${device} passkey` : "Passkey";
}
