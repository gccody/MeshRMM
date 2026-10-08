import assert from "node:assert/strict";
import test from "node:test";
import {
  DEFAULT_BLACKOUT_MESSAGE,
  DEFAULT_CONNECTION_APPROVAL_MESSAGE,
  DEFAULT_CONNECTION_NOTIFICATION_MESSAGE,
  IDLE_DISCONNECT_MINUTES,
  SETTINGS_TABS,
  settingsBody,
  draftFromSettings,
  draftMatchesSettings,
  formatIdleDisconnect,
  isBlackoutMessageValid,
  isConnectionApprovalDraftValid,
  isConnectionApprovalLockIdleValid,
  isConnectionApprovalMessageValid,
  isConnectionApprovalTimeoutValid,
  isConnectionNotificationMessageValid,
  isInstanceNameValid,
  settingsTabForKey,
} from "../features/settings/general-settings.ts";
import { DEFAULT_IDLE_TIMEOUT_MINUTES } from "../features/session/idle-session.ts";

const company = {
  instance_name: "Acme IT",
  updated_at: 1,
  dashboard_idle_timeout_minutes: 30,
  blackout_message: "Back soon, {user_name}.",
  display_border: false,
  prevent_idle_lock: false,
  allow_idle_override: true,
  idle_disconnect_minutes: 15,
  allow_idle_disconnect_override: false,
  clear_clipboard_on_close: false,
  allow_clear_clipboard_override: false,
  session_banner: false,
  connection_notification: false,
  background_connection_notification: true,
  connection_notification_message: "{user_name} is here.",
  connection_approval: true,
  connection_approval_message: "{user_name} asks to connect.",
  connection_approval_timeout_seconds: 45,
  connection_approval_lock_idle_seconds: 0,
};

test("drafts start from the saved settings, or the defaults before they load", () => {
  assert.deepEqual(draftFromSettings(company), {
    instanceName: "Acme IT",
    idleTimeoutMinutes: 30,
    blackoutMessage: "Back soon, {user_name}.",
    displayBorder: false,
    preventIdleLock: false,
    allowIdleOverride: true,
    idleDisconnectMinutes: 15,
    allowIdleDisconnectOverride: false,
    clearClipboardOnClose: false,
    allowClearClipboardOverride: false,
    sessionBanner: false,
    connectionNotification: false,
    backgroundConnectionNotification: true,
    connectionNotificationMessage: "{user_name} is here.",
    connectionApproval: true,
    connectionApprovalMessage: "{user_name} asks to connect.",
    connectionApprovalTimeoutSeconds: 45,
    connectionApprovalLockIdleSeconds: 0,
  });
  assert.deepEqual(draftFromSettings(null), {
    instanceName: "",
    idleTimeoutMinutes: DEFAULT_IDLE_TIMEOUT_MINUTES,
    blackoutMessage: DEFAULT_BLACKOUT_MESSAGE,
    displayBorder: true,
    preventIdleLock: true,
    allowIdleOverride: true,
    idleDisconnectMinutes: null,
    allowIdleDisconnectOverride: true,
    clearClipboardOnClose: true,
    allowClearClipboardOverride: true,
    sessionBanner: true,
    connectionNotification: true,
    backgroundConnectionNotification: false,
    connectionNotificationMessage: DEFAULT_CONNECTION_NOTIFICATION_MESSAGE,
    connectionApproval: false,
    connectionApprovalMessage: DEFAULT_CONNECTION_APPROVAL_MESSAGE,
    connectionApprovalTimeoutSeconds: 30,
    connectionApprovalLockIdleSeconds: 60,
  });
});

test("saving is offered only when a draft differs from the saved settings", () => {
  const draft = draftFromSettings(company);
  assert.equal(draftMatchesSettings(draft, company), true);
  for (const change of [
    { instanceName: "Acme Support" },
    { idleTimeoutMinutes: 60 },
    { blackoutMessage: "Maintenance" },
    { displayBorder: true },
    { preventIdleLock: true },
    { allowIdleOverride: false },
    { idleDisconnectMinutes: null },
    { idleDisconnectMinutes: 30 },
    { allowIdleDisconnectOverride: true },
    { clearClipboardOnClose: true },
    { allowClearClipboardOverride: true },
    { sessionBanner: true },
    { connectionNotification: true },
    { backgroundConnectionNotification: false },
    { connectionNotificationMessage: "Hello" },
    { connectionApproval: false },
    { connectionApprovalMessage: "Hello" },
    { connectionApprovalTimeoutSeconds: 30 },
    { connectionApprovalLockIdleSeconds: 60 },
  ]) {
    assert.equal(draftMatchesSettings({ ...draft, ...change }, company), false, JSON.stringify(change));
  }
});

test("the blackout message needs visible text and at most 2 KB of UTF-8", () => {
  assert.equal(isBlackoutMessageValid(DEFAULT_BLACKOUT_MESSAGE), true);
  assert.equal(isBlackoutMessageValid(""), false);
  assert.equal(isBlackoutMessageValid("  \n "), false);
  assert.equal(isBlackoutMessageValid("a".repeat(2048)), true);
  assert.equal(isBlackoutMessageValid("a".repeat(2049)), false);
  // "é" is two bytes in UTF-8, so 1,025 of them exceed the limit.
  assert.equal(isBlackoutMessageValid("é".repeat(1024)), true);
  assert.equal(isBlackoutMessageValid("é".repeat(1025)), false);
});

test("the connection notification message needs visible text and at most 512 bytes of UTF-8", () => {
  assert.equal(isConnectionNotificationMessageValid(DEFAULT_CONNECTION_NOTIFICATION_MESSAGE), true);
  assert.equal(isConnectionNotificationMessageValid(" \n "), false);
  assert.equal(isConnectionNotificationMessageValid("a".repeat(512)), true);
  assert.equal(isConnectionNotificationMessageValid("a".repeat(513)), false);
  assert.equal(isConnectionNotificationMessageValid("é".repeat(256)), true);
  assert.equal(isConnectionNotificationMessageValid("é".repeat(257)), false);
});

test("approval settings need a short message and whole seconds in range", () => {
  assert.equal(DEFAULT_CONNECTION_APPROVAL_MESSAGE, "{user_name} would like to connect.");
  assert.equal(isConnectionApprovalMessageValid(DEFAULT_CONNECTION_APPROVAL_MESSAGE), true);
  assert.equal(isConnectionApprovalMessageValid(" "), false);
  assert.equal(isConnectionApprovalMessageValid("é".repeat(257)), false);
  for (const [seconds, valid] of [[4, false], [5, true], [300, true], [301, false], [30.5, false], [Number.NaN, false]]) {
    assert.equal(isConnectionApprovalTimeoutValid(seconds), valid, String(seconds));
  }
  for (const [seconds, valid] of [[-1, false], [0, true], [3600, true], [3601, false], [Number.NaN, false]]) {
    assert.equal(isConnectionApprovalLockIdleValid(seconds), valid, String(seconds));
  }
  const draft = draftFromSettings(company);
  assert.equal(isConnectionApprovalDraftValid(draft), true);
  assert.equal(isConnectionApprovalDraftValid({ ...draft, connectionApprovalTimeoutSeconds: Number.NaN }), false);
  assert.equal(isConnectionApprovalDraftValid({ ...draft, connectionApprovalMessage: "" }), false);
});

test("the instance name needs 1 to 120 characters without control characters", () => {
  assert.equal(isInstanceNameValid("Acme IT"), true);
  assert.equal(isInstanceNameValid("   "), false);
  assert.equal(isInstanceNameValid("a".repeat(120)), true);
  assert.equal(isInstanceNameValid("a".repeat(121)), false);
  assert.equal(isInstanceNameValid("Acme\tIT"), false);
  // Characters, not UTF-16 units: an emoji counts once.
  assert.equal(isInstanceNameValid("😀".repeat(120)), true);
});

test("the saved body uses the server's field names", () => {
  assert.deepEqual(settingsBody({ ...draftFromSettings(company), instanceName: "  Acme Support " }), {
    instance_name: "Acme Support",
    dashboard_idle_timeout_minutes: 30,
    blackout_message: "Back soon, {user_name}.",
    display_border: false,
    prevent_idle_lock: false,
    allow_idle_override: true,
    idle_disconnect_minutes: 15,
    allow_idle_disconnect_override: false,
    clear_clipboard_on_close: false,
    allow_clear_clipboard_override: false,
    session_banner: false,
    connection_notification: false,
    background_connection_notification: true,
    connection_notification_message: "{user_name} is here.",
    connection_approval: true,
    connection_approval_message: "{user_name} asks to connect.",
    connection_approval_timeout_seconds: 45,
    connection_approval_lock_idle_seconds: 0,
  });
  // Never is sent as null, which the server requires rather than defaults.
  assert.equal(
    settingsBody({ ...draftFromSettings(company), idleDisconnectMinutes: null }).idle_disconnect_minutes,
    null,
  );
});

test("idle disconnect choices read as durations, with never first", () => {
  assert.deepEqual(
    [null, ...IDLE_DISCONNECT_MINUTES].map(formatIdleDisconnect),
    ["Never", "5 minutes", "10 minutes", "15 minutes", "30 minutes", "1 hour", "2 hours", "4 hours", "8 hours"],
  );
});

test("arrow keys wrap between settings tabs and Home/End jump to the ends", () => {
  const last = SETTINGS_TABS.length - 1;
  assert.equal(settingsTabForKey(0, "ArrowRight"), 1);
  assert.equal(settingsTabForKey(last, "ArrowRight"), 0);
  assert.equal(settingsTabForKey(0, "ArrowLeft"), last);
  assert.equal(settingsTabForKey(1, "Home"), 0);
  assert.equal(settingsTabForKey(0, "End"), last);
  assert.equal(settingsTabForKey(0, "Enter"), null);
  assert.equal(settingsTabForKey(0, "ArrowDown"), null);
});
