import assert from "node:assert/strict";
import test from "node:test";
import {
  DEFAULT_BLACKOUT_MESSAGE,
  SETTINGS_TABS,
  companySettingsBody,
  draftFromCompany,
  draftMatchesCompany,
  isBlackoutMessageValid,
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
};

test("drafts start from the saved settings, or the defaults before the account loads", () => {
  assert.deepEqual(draftFromCompany(company), {
    idleTimeoutMinutes: 30,
    blackoutMessage: "Back soon, {user_name}.",
    displayBorder: false,
    preventIdleLock: false,
    allowIdleOverride: true,
  });
  assert.deepEqual(draftFromCompany(null), {
    idleTimeoutMinutes: DEFAULT_IDLE_TIMEOUT_MINUTES,
    blackoutMessage: DEFAULT_BLACKOUT_MESSAGE,
    displayBorder: true,
    preventIdleLock: true,
    allowIdleOverride: true,
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

test("the saved body uses the control plane's field names", () => {
  assert.deepEqual(companySettingsBody(draftFromCompany(company)), {
    dashboard_idle_timeout_minutes: 30,
    blackout_message: "Back soon, {user_name}.",
    display_border: false,
    prevent_idle_lock: false,
    allow_idle_override: true,
  });
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
