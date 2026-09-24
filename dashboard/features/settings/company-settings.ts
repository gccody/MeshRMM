// Company settings drafts, validation and tab navigation. Imports carry the
// .ts extension so node:test can load this module directly.
import { DEFAULT_IDLE_TIMEOUT_MINUTES } from "../session/idle-session.ts";
import type { Company } from "../workspace/types";

export const DEFAULT_BLACKOUT_MESSAGE = "This machine is under maintenance by {user_name}.";
export const MAX_BLACKOUT_MESSAGE_BYTES = 2048;

export const SETTINGS_TABS = [
  { id: "dashboard-security", label: "Dashboard security" },
  { id: "remote-sessions", label: "Remote sessions" },
  { id: "blackout", label: "Blackout message" },
] as const;
export type SettingsTab = (typeof SETTINGS_TABS)[number]["id"];

export type CompanySettingsDraft = {
  idleTimeoutMinutes: number;
  blackoutMessage: string;
  displayBorder: boolean;
  preventIdleLock: boolean;
  allowIdleOverride: boolean;
};

// The saved values, or the defaults a company starts with.
export function draftFromCompany(company: Company | null | undefined): CompanySettingsDraft {
  return {
    idleTimeoutMinutes: company?.dashboard_idle_timeout_minutes ?? DEFAULT_IDLE_TIMEOUT_MINUTES,
    blackoutMessage: company?.blackout_message ?? DEFAULT_BLACKOUT_MESSAGE,
    displayBorder: company?.display_border ?? true,
    preventIdleLock: company?.prevent_idle_lock ?? true,
    allowIdleOverride: company?.allow_idle_override ?? true,
  };
}

export function draftMatchesCompany(draft: CompanySettingsDraft, company: Company | null | undefined) {
  const saved = draftFromCompany(company);
  return (
    draft.idleTimeoutMinutes === saved.idleTimeoutMinutes &&
    draft.blackoutMessage === saved.blackoutMessage &&
    draft.displayBorder === saved.displayBorder &&
    draft.preventIdleLock === saved.preventIdleLock &&
    draft.allowIdleOverride === saved.allowIdleOverride
  );
}

// The server stores at most 2 KB of UTF-8 and requires visible text.
export function isBlackoutMessageValid(message: string) {
  return Boolean(message.trim()) && new TextEncoder().encode(message).length <= MAX_BLACKOUT_MESSAGE_BYTES;
}

// The request body for PUT /v1/company/settings.
export function companySettingsBody(draft: CompanySettingsDraft) {
  return {
    dashboard_idle_timeout_minutes: draft.idleTimeoutMinutes,
    blackout_message: draft.blackoutMessage,
    display_border: draft.displayBorder,
    prevent_idle_lock: draft.preventIdleLock,
    allow_idle_override: draft.allowIdleOverride,
  };
}

// Arrow keys wrap between tabs; Home and End jump to the ends. Other keys
// return null and keep their default behavior.
export function settingsTabForKey(index: number, key: string): number | null {
  const count = SETTINGS_TABS.length;
  if (key === "ArrowRight") return (index + 1) % count;
  if (key === "ArrowLeft") return (index + count - 1) % count;
  if (key === "Home") return 0;
  if (key === "End") return count - 1;
  return null;
}
