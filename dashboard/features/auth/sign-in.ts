// Ways to sign in besides a password: single sign-on and passkeys, and the
// second step of a password sign-in. Imports carry the .ts extension so
// node:test can load this module directly.
import { safeNextPath } from "./next-path.ts";

// Starts single sign-on, returning to `next` (a path on this site) after.
// It is a page the browser loads, not an API call: the identity provider
// takes over the window.
export function ssoStartPath(next: string | null | undefined) {
  return `/v1/auth/sso/start?${new URLSearchParams({ next: safeNextPath(next) })}`;
}

const SSO_ERRORS: Record<string, string> = {
  unavailable: "Single sign-on isn't set up on this server. Sign in with your password or a passkey.",
  failed: "Your identity provider couldn't sign you in. Try again, or ask an administrator to check the server log.",
  expired: "The sign-in took too long, or started in another browser. Try again.",
  denied: "Your identity provider didn't sign you in.",
  no_account: "No MeshRMM account matches your identity provider account. Ask an administrator to invite you.",
  no_email: "Your identity provider didn't share an email address, so MeshRMM can't tell which account is yours.",
  email_unverified: "Your identity provider hasn't verified your email address, so MeshRMM can't sign you in with it.",
  conflict: "Your MeshRMM account is linked to a different identity at your provider. An administrator can unlink it.",
  account_disabled: "Your account is disabled. Ask an administrator to enable it.",
  rate_limited: "Too many sign-in attempts. Wait a few minutes and try again.",
};

// What the sign-in page says when single sign-on sent the browser back with
// `?sso_error=<code>`; null without a code.
export function ssoErrorMessage(code: string | null | undefined): string | null {
  if (!code) return null;
  return SSO_ERRORS[code] ?? SSO_ERRORS.failed;
}

export type SecondFactorMethod = "totp" | "passkey" | "recovery_code";

// What the second step asks for first: the authenticator app when the user
// has one, else a passkey when one can be used here, else a recovery code.
export function firstSecondFactor(methods: readonly string[], passkeyUsable: boolean): SecondFactorMethod {
  if (methods.includes("totp")) return "totp";
  if (passkeyUsable && methods.includes("passkey")) return "passkey";
  return "recovery_code";
}

// How a session was signed in, as the account page names it.
export function signInMethodLabel(method: string) {
  switch (method) {
    case "password": return "Password";
    case "passkey": return "Passkey";
    case "oidc": return "SSO";
    default: return method;
  }
}
