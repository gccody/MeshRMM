import assert from "node:assert/strict";
import test from "node:test";
import { defaultLifetime, expiryText, isExpired, lastUsedText, lifetimeChoices, newTokenProblem, scopeText } from "../features/api-tokens/model.ts";
import { eventLabel, targetLabel } from "../features/audit/model.ts";

const DAY = 24 * 60 * 60_000;
const NOW = Date.UTC(2026, 9, 22, 12);

test("lifetimes stay within the server's cap", () => {
  assert.deepEqual(lifetimeChoices(365).map((choice) => choice.days), [7, 30, 90, 365]);
  assert.deepEqual(lifetimeChoices(60).map((choice) => choice.days), [7, 30, 60], "the cap itself is offered");
  assert.deepEqual(lifetimeChoices(null).map((choice) => choice.days), [7, 30, 90, 365, null], "no cap allows never");
  assert.equal(lifetimeChoices(null).at(-1).label, "Never");
  assert.deepEqual(lifetimeChoices(1), [{ days: 1, label: "1 day" }]);
  assert.equal(defaultLifetime(365), 30);
  assert.equal(defaultLifetime(7), 7);
  assert.equal(defaultLifetime(null), 30);
});

test("tokens say when they expire, were used and what they may do", () => {
  assert.equal(expiryText({ expires_at: null }, NOW), "Never expires");
  assert.equal(expiryText({ expires_at: NOW + 3 * DAY }, NOW), "Expires in 3 days");
  assert.equal(expiryText({ expires_at: NOW - 2 * DAY }, NOW), "Expired 2 days ago");
  assert.equal(isExpired({ expires_at: NOW }, NOW), true);
  assert.equal(isExpired({ expires_at: null }, NOW), false);
  assert.equal(lastUsedText({ last_used_at: null, last_used_ip: null }, NOW), "Never used");
  assert.equal(lastUsedText({ last_used_at: NOW - 5 * 60_000, last_used_ip: "203.0.113.7" }, NOW), "Used 5 minutes ago from 203.0.113.7");
  assert.equal(scopeText({ permissions: null }), "All of its user's permissions");
  assert.equal(scopeText({ permissions: ["devices.view", "audit.view"] }), "devices.view, audit.view");
  assert.equal(scopeText({ permissions: ["devices.view", "audit.view", "alerts.view", "alerts.respond"] }), "4 permissions");
});

test("a new token needs a name and, when limited, a permission", () => {
  assert.match(newTokenProblem(" ", false, new Set()), /Name/u);
  assert.match(newTokenProblem("Sync", true, new Set()), /at least one/u);
  assert.equal(newTokenProblem("Sync", true, new Set(["devices.view"])), null);
  assert.equal(newTokenProblem("Sync", false, new Set()), null);
});

test("the audit log words token events", () => {
  assert.equal(eventLabel({ action: "api_token.create", metadata: { name: "Sync" } }), "Made an API token");
  assert.equal(targetLabel({ target_type: "api_token", target_id: "t1", metadata: { name: "Sync" } }), "API token: Sync");
});
