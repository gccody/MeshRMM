// The server's settings: drafts, validation and tab navigation. Imports
// carry the .ts extension so node:test can load this module directly.
import { DEFAULT_IDLE_TIMEOUT_MINUTES } from "../session/idle-session.ts";

// GET /v1/settings
export type GeneralSettings = {
  instance_name: string;
  dashboard_idle_timeout_minutes: number;
  blackout_message: string;
  display_border: boolean;
  prevent_idle_lock: boolean;
  allow_idle_override: boolean;
  session_banner: boolean;
  connection_notification: boolean;
  background_connection_notification: boolean;
  connection_notification_message: string;
  // Minutes a remote session may sit idle before it is disconnected; null never disconnects.
  idle_disconnect_minutes: number | null;
  allow_idle_disconnect_override: boolean;
  clear_clipboard_on_close: boolean;
  allow_clear_clipboard_override: boolean;
  connection_approval: boolean;
  connection_approval_message: string;
  connection_approval_timeout_seconds: number;
  connection_approval_lock_idle_seconds: number;
  updated_at: number;
};

// The server's limit on the instance name, in characters.
export const MAX_INSTANCE_NAME_LENGTH = 120;

export const DEFAULT_BLACKOUT_MESSAGE = "This machine is under maintenance by {user_name}.";
export const MAX_BLACKOUT_MESSAGE_BYTES = 2048;
export const DEFAULT_CONNECTION_NOTIFICATION_MESSAGE = "{user_name} has connected to this computer.";
export const MAX_CONNECTION_NOTIFICATION_MESSAGE_BYTES = 512;
export const DEFAULT_CONNECTION_APPROVAL_MESSAGE = "{user_name} would like to connect.";
export const MAX_CONNECTION_APPROVAL_MESSAGE_BYTES = 512;
export const DEFAULT_CONNECTION_APPROVAL_TIMEOUT_SECONDS = 30;
export const MIN_CONNECTION_APPROVAL_TIMEOUT_SECONDS = 5;
export const MAX_CONNECTION_APPROVAL_TIMEOUT_SECONDS = 300;
export const DEFAULT_CONNECTION_APPROVAL_LOCK_IDLE_SECONDS = 60;
export const MAX_CONNECTION_APPROVAL_LOCK_IDLE_SECONDS = 3600;

// The server accepts only these idle times, in minutes, or null for never.
export const IDLE_DISCONNECT_MINUTES = [5, 10, 15, 30, 60, 120, 240, 480] as const;

export function formatIdleDisconnect(minutes: number | null) {
  if (minutes === null) return "Never";
  if (minutes < 60) return `${minutes} minutes`;
  const hours = minutes / 60;
  return `${hours} ${hours === 1 ? "hour" : "hours"}`;
}

export const SETTINGS_TABS = [
  { id: "general", label: "General" },
  { id: "remote-sessions", label: "Remote sessions" },
  { id: "blackout", label: "Blackout message" },
  { id: "connection-notification", label: "Connection notification" },
  { id: "connection-approval", label: "Connection approval" },
] as const;
export type SettingsTab = (typeof SETTINGS_TABS)[number]["id"];

export type SettingsDraft = {
  instanceName: string;
  idleTimeoutMinutes: number;
  blackoutMessage: string;
  displayBorder: boolean;
  preventIdleLock: boolean;
  allowIdleOverride: boolean;
  idleDisconnectMinutes: number | null;
  allowIdleDisconnectOverride: boolean;
  clearClipboardOnClose: boolean;
  allowClearClipboardOverride: boolean;
  sessionBanner: boolean;
  connectionNotification: boolean;
  backgroundConnectionNotification: boolean;
  connectionNotificationMessage: string;
  connectionApproval: boolean;
  connectionApprovalMessage: string;
  connectionApprovalTimeoutSeconds: number;
  connectionApprovalLockIdleSeconds: number;
};

// The saved values, or the defaults a server starts with.
export function draftFromSettings(settings: GeneralSettings | null | undefined): SettingsDraft {
  return {
    instanceName: settings?.instance_name ?? "",
    idleTimeoutMinutes: settings?.dashboard_idle_timeout_minutes ?? DEFAULT_IDLE_TIMEOUT_MINUTES,
    blackoutMessage: settings?.blackout_message ?? DEFAULT_BLACKOUT_MESSAGE,
    displayBorder: settings?.display_border ?? true,
    preventIdleLock: settings?.prevent_idle_lock ?? true,
    allowIdleOverride: settings?.allow_idle_override ?? true,
    idleDisconnectMinutes: settings?.idle_disconnect_minutes ?? null,
    allowIdleDisconnectOverride: settings?.allow_idle_disconnect_override ?? true,
    clearClipboardOnClose: settings?.clear_clipboard_on_close ?? true,
    allowClearClipboardOverride: settings?.allow_clear_clipboard_override ?? true,
    sessionBanner: settings?.session_banner ?? true,
    connectionNotification: settings?.connection_notification ?? true,
    backgroundConnectionNotification: settings?.background_connection_notification ?? false,
    connectionNotificationMessage: settings?.connection_notification_message ?? DEFAULT_CONNECTION_NOTIFICATION_MESSAGE,
    connectionApproval: settings?.connection_approval ?? false,
    connectionApprovalMessage: settings?.connection_approval_message ?? DEFAULT_CONNECTION_APPROVAL_MESSAGE,
    connectionApprovalTimeoutSeconds: settings?.connection_approval_timeout_seconds ?? DEFAULT_CONNECTION_APPROVAL_TIMEOUT_SECONDS,
    connectionApprovalLockIdleSeconds: settings?.connection_approval_lock_idle_seconds ?? DEFAULT_CONNECTION_APPROVAL_LOCK_IDLE_SECONDS,
  };
}

export function draftMatchesSettings(draft: SettingsDraft, settings: GeneralSettings | null | undefined) {
  const saved = draftFromSettings(settings);
  return (
    draft.instanceName === saved.instanceName &&
    draft.idleTimeoutMinutes === saved.idleTimeoutMinutes &&
    draft.blackoutMessage === saved.blackoutMessage &&
    draft.displayBorder === saved.displayBorder &&
    draft.preventIdleLock === saved.preventIdleLock &&
    draft.allowIdleOverride === saved.allowIdleOverride &&
    draft.idleDisconnectMinutes === saved.idleDisconnectMinutes &&
    draft.allowIdleDisconnectOverride === saved.allowIdleDisconnectOverride &&
    draft.clearClipboardOnClose === saved.clearClipboardOnClose &&
    draft.allowClearClipboardOverride === saved.allowClearClipboardOverride &&
    draft.sessionBanner === saved.sessionBanner &&
    draft.connectionNotification === saved.connectionNotification &&
    draft.backgroundConnectionNotification === saved.backgroundConnectionNotification &&
    draft.connectionNotificationMessage === saved.connectionNotificationMessage &&
    draft.connectionApproval === saved.connectionApproval &&
    draft.connectionApprovalMessage === saved.connectionApprovalMessage &&
    draft.connectionApprovalTimeoutSeconds === saved.connectionApprovalTimeoutSeconds &&
    draft.connectionApprovalLockIdleSeconds === saved.connectionApprovalLockIdleSeconds
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

// The approval prompt is a small popup too, with the same limit.
export function isConnectionApprovalMessageValid(message: string) {
  return isTemplateValid(message, MAX_CONNECTION_APPROVAL_MESSAGE_BYTES);
}

function isWholeNumberBetween(value: number, min: number, max: number) {
  return Number.isInteger(value) && value >= min && value <= max;
}

// Seconds the user has to answer before the connection is accepted for them.
export function isConnectionApprovalTimeoutValid(seconds: number) {
  return isWholeNumberBetween(seconds, MIN_CONNECTION_APPROVAL_TIMEOUT_SECONDS, MAX_CONNECTION_APPROVAL_TIMEOUT_SECONDS);
}

// Seconds a locked computer must have been idle to accept at once; 0 accepts
// whenever it is locked.
export function isConnectionApprovalLockIdleValid(seconds: number) {
  return isWholeNumberBetween(seconds, 0, MAX_CONNECTION_APPROVAL_LOCK_IDLE_SECONDS);
}

// The server trims the name and needs 1 to 120 characters, none of them
// control characters.
export function isInstanceNameValid(name: string) {
  const trimmed = name.trim();
  // eslint-disable-next-line no-control-regex
  return Boolean(trimmed) && Array.from(trimmed).length <= MAX_INSTANCE_NAME_LENGTH && !/[\u0000-\u001f\u007f-\u009f]/u.test(trimmed);
}

// Whether every approval setting can be saved.
export function isConnectionApprovalDraftValid(draft: SettingsDraft) {
  return isConnectionApprovalMessageValid(draft.connectionApprovalMessage)
    && isConnectionApprovalTimeoutValid(draft.connectionApprovalTimeoutSeconds)
    && isConnectionApprovalLockIdleValid(draft.connectionApprovalLockIdleSeconds);
}

// The request body for PATCH /v1/settings.
export function settingsBody(draft: SettingsDraft) {
  return {
    instance_name: draft.instanceName.trim(),
    dashboard_idle_timeout_minutes: draft.idleTimeoutMinutes,
    blackout_message: draft.blackoutMessage,
    display_border: draft.displayBorder,
    prevent_idle_lock: draft.preventIdleLock,
    allow_idle_override: draft.allowIdleOverride,
    idle_disconnect_minutes: draft.idleDisconnectMinutes,
    allow_idle_disconnect_override: draft.allowIdleDisconnectOverride,
    clear_clipboard_on_close: draft.clearClipboardOnClose,
    allow_clear_clipboard_override: draft.allowClearClipboardOverride,
    session_banner: draft.sessionBanner,
    connection_notification: draft.connectionNotification,
    background_connection_notification: draft.backgroundConnectionNotification,
    connection_notification_message: draft.connectionNotificationMessage,
    connection_approval: draft.connectionApproval,
    connection_approval_message: draft.connectionApprovalMessage,
    connection_approval_timeout_seconds: draft.connectionApprovalTimeoutSeconds,
    connection_approval_lock_idle_seconds: draft.connectionApprovalLockIdleSeconds,
  };
}

// Arrow keys wrap between a list's tabs, which may be laid out in a row or a
// column; Home and End jump to the ends. Other keys return null and keep
// their default behavior.
export function tabForKey(index: number, count: number, key: string): number | null {
  if (key === "ArrowRight" || key === "ArrowDown") return (index + 1) % count;
  if (key === "ArrowLeft" || key === "ArrowUp") return (index + count - 1) % count;
  if (key === "Home") return 0;
  if (key === "End") return count - 1;
  return null;
}
