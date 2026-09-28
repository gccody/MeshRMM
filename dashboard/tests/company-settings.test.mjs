import assert from "node:assert/strict";
import test from "node:test";
import {
  DEFAULT_BLACKOUT_MESSAGE,
  DEFAULT_CONNECTION_NOTIFICATION_MESSAGE,
  IDLE_DISCONNECT_MINUTES,
  SETTINGS_TABS,
  companySettingsBody,
  draftFromCompany,
  draftMatchesCompany,
  formatIdleDisconnect,
  isBlackoutMessageValid,
  isConnectionNotificationMessageValid,
  settingsTabForKey,
} from "../features/settings/company-settings.ts";
import { DEFAULT_IDLE_TIMEOUT_MINUTES } from "../features/session/idle-session.ts";

const company = {
  id: "c1",
  name: "Acme",
  slug: "acme",
  status: "active",
  dashboard_idle_timeout_minutes: 30,
  blackout_message: "Back soon, {user_name}.",
  display_border: false,
  prevent_idle_lock: false,
  allow_idle_override: true,
  idle_disconnect_minutes: 15,
  allow_idle_disconnect_override: false,
  session_banner: false,
  connection_notification: false,
  background_connection_notification: true,
  connection_notification_message: "{user_name} is here.",
};

test("drafts start from the saved settings, or the defaults before the account loads", () => {
  assert.deepEqual(draftFromCompany(company), {
    idleTimeoutMinutes: 30,
    blackoutMessage: "Back soon, {user_name}.",
    displayBorder: false,
    preventIdleLock: false,
    allowIdleOverride: true,
    idleDisconnectMinutes: 15,
    allowIdleDisconnectOverride: false,
    sessionBanner: false,
    connectionNotification: false,
    backgroundConnectionNotification: true,
    connectionNotificationMessage: "{user_name} is here.",
  });
  assert.deepEqual(draftFromCompany(null), {
    idleTimeoutMinutes: DEFAULT_IDLE_TIMEOUT_MINUTES,
    blackoutMessage: DEFAULT_BLACKOUT_MESSAGE,
    displayBorder: true,
    preventIdleLock: true,
    allowIdleOverride: true,
    idleDisconnectMinutes: null,
    allowIdleDisconnectOverride: true,
    sessionBanner: true,
    connectionNotification: true,
    backgroundConnectionNotification: false,
    connectionNotificationMessage: DEFAULT_CONNECTION_NOTIFICATION_MESSAGE,
  });
});

test("saving is offered only when a draft differs from the saved settings", () => {
  const draft = draftFromCompany(company);
  assert.equal(draftMatchesCompany(draft, company), true);
  for (const change of [
    { idleTimeoutMinutes: 60 },
    { blackoutMessage: "Maintenance" },
    { displayBorder: true },
    { preventIdleLock: true },
    { allowIdleOverride: false },
    { idleDisconnectMinutes: null },
    { idleDisconnectMinutes: 30 },
    { allowIdleDisconnectOverride: true },
    { sessionBanner: true },
    { connectionNotification: true },
    { backgroundConnectionNotification: false },
    { connectionNotificationMessage: "Hello" },
  ]) {
    assert.equal(draftMatchesCompany({ ...draft, ...change }, company), false, JSON.stringify(change));
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

test("the saved body uses the control plane's field names", () => {
  assert.deepEqual(companySettingsBody(draftFromCompany(company)), {
    dashboard_idle_timeout_minutes: 30,
    blackout_message: "Back soon, {user_name}.",
    display_border: false,
    prevent_idle_lock: false,
    allow_idle_override: true,
    idle_disconnect_minutes: 15,
    allow_idle_disconnect_override: false,
    session_banner: false,
    connection_notification: false,
    background_connection_notification: true,
    connection_notification_message: "{user_name} is here.",
  });
  // Never is sent as null, which the server requires rather than defaults.
  assert.equal(
    companySettingsBody({ ...draftFromCompany(company), idleDisconnectMinutes: null }).idle_disconnect_minutes,
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
