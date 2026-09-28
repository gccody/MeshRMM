// Company settings drafts, validation and tab navigation. Imports carry the
// .ts extension so node:test can load this module directly.
import { DEFAULT_IDLE_TIMEOUT_MINUTES } from "../session/idle-session.ts";
import type { Company } from "../workspace/types";

export const DEFAULT_BLACKOUT_MESSAGE = "This machine is under maintenance by {user_name}.";
export const MAX_BLACKOUT_MESSAGE_BYTES = 2048;
export const DEFAULT_CONNECTION_NOTIFICATION_MESSAGE = "{user_name} has connected to this computer.";
export const MAX_CONNECTION_NOTIFICATION_MESSAGE_BYTES = 512;

// The server accepts only these idle times, in minutes, or null for never.
export const IDLE_DISCONNECT_MINUTES = [5, 10, 15, 30, 60, 120, 240, 480] as const;

export function formatIdleDisconnect(minutes: number | null) {
  if (minutes === null) return "Never";
  if (minutes < 60) return `${minutes} minutes`;
  const hours = minutes / 60;
  return `${hours} ${hours === 1 ? "hour" : "hours"}`;
}

export const SETTINGS_TABS = [
  { id: "dashboard-security", label: "Dashboard security" },
  { id: "remote-sessions", label: "Remote sessions" },
  { id: "blackout", label: "Blackout message" },
  { id: "connection-notification", label: "Connection notification" },
] as const;
export type SettingsTab = (typeof SETTINGS_TABS)[number]["id"];

export type CompanySettingsDraft = {
  idleTimeoutMinutes: number;
  blackoutMessage: string;
  displayBorder: boolean;
  preventIdleLock: boolean;
  allowIdleOverride: boolean;
  idleDisconnectMinutes: number | null;
  allowIdleDisconnectOverride: boolean;
  sessionBanner: boolean;
  connectionNotification: boolean;
  backgroundConnectionNotification: boolean;
  connectionNotificationMessage: string;
};

// The saved values, or the defaults a company starts with.
export function draftFromCompany(company: Company | null | undefined): CompanySettingsDraft {
  return {
    idleTimeoutMinutes: company?.dashboard_idle_timeout_minutes ?? DEFAULT_IDLE_TIMEOUT_MINUTES,
    blackoutMessage: company?.blackout_message ?? DEFAULT_BLACKOUT_MESSAGE,
    displayBorder: company?.display_border ?? true,
    preventIdleLock: company?.prevent_idle_lock ?? true,
    allowIdleOverride: company?.allow_idle_override ?? true,
    idleDisconnectMinutes: company?.idle_disconnect_minutes ?? null,
    allowIdleDisconnectOverride: company?.allow_idle_disconnect_override ?? true,
    sessionBanner: company?.session_banner ?? true,
    connectionNotification: company?.connection_notification ?? true,
    backgroundConnectionNotification: company?.background_connection_notification ?? false,
    connectionNotificationMessage: company?.connection_notification_message ?? DEFAULT_CONNECTION_NOTIFICATION_MESSAGE,
  };
}

export function draftMatchesCompany(draft: CompanySettingsDraft, company: Company | null | undefined) {
  const saved = draftFromCompany(company);
  return (
    draft.idleTimeoutMinutes === saved.idleTimeoutMinutes &&
    draft.blackoutMessage === saved.blackoutMessage &&
    draft.displayBorder === saved.displayBorder &&
    draft.preventIdleLock === saved.preventIdleLock &&
    draft.allowIdleOverride === saved.allowIdleOverride &&
    draft.idleDisconnectMinutes === saved.idleDisconnectMinutes &&
    draft.allowIdleDisconnectOverride === saved.allowIdleDisconnectOverride &&
    draft.sessionBanner === saved.sessionBanner &&
    draft.connectionNotification === saved.connectionNotification &&
    draft.backgroundConnectionNotification === saved.backgroundConnectionNotification &&
    draft.connectionNotificationMessage === saved.connectionNotificationMessage
  );
}

function isTemplateValid(message: string, maxBytes: number) {
  return Boolean(message.trim()) && new TextEncoder().encode(message).length <= maxBytes;
}

// The server stores at most 2 KB of UTF-8 and requires visible text.
export function isBlackoutMessageValid(message: string) {
  return isTemplateValid(message, MAX_BLACKOUT_MESSAGE_BYTES);
}

// The notification is a small popup, so the server allows only 512 bytes.
export function isConnectionNotificationMessageValid(message: string) {
  return isTemplateValid(message, MAX_CONNECTION_NOTIFICATION_MESSAGE_BYTES);
}

// The request body for PUT /v1/company/settings.
export function companySettingsBody(draft: CompanySettingsDraft) {
  return {
    dashboard_idle_timeout_minutes: draft.idleTimeoutMinutes,
    blackout_message: draft.blackoutMessage,
    display_border: draft.displayBorder,
    prevent_idle_lock: draft.preventIdleLock,
    allow_idle_override: draft.allowIdleOverride,
    idle_disconnect_minutes: draft.idleDisconnectMinutes,
    allow_idle_disconnect_override: draft.allowIdleDisconnectOverride,
    session_banner: draft.sessionBanner,
    connection_notification: draft.connectionNotification,
    background_connection_notification: draft.backgroundConnectionNotification,
    connection_notification_message: draft.connectionNotificationMessage,
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
