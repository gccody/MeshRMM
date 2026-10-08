import assert from "node:assert/strict";
import test from "node:test";
import { VIEW_COPY, VIEW_PATHS, canView, homeView, viewForPath } from "../features/workspace/views.ts";

const account = (permissions, is_administrator = false) => ({ permissions, is_administrator });

test("maps each workspace route to its view", () => {
  assert.equal(viewForPath("/"), "devices");
  assert.equal(viewForPath("/settings"), "settings");
  assert.equal(viewForPath("/roles"), "roles");
  assert.equal(viewForPath("/audit"), "audit");
  for (const [view, path] of Object.entries(VIEW_PATHS)) {
    assert.equal(viewForPath(path), view);
    assert.ok(VIEW_COPY[view].title);
  }
});

test("ignores a trailing slash", () => {
  assert.equal(viewForPath("/settings/"), "settings");
  assert.equal(viewForPath("/users//"), "users");
  assert.equal(viewForPath("//"), "devices");
});

test("unknown paths have no view", () => {
  assert.equal(viewForPath("/unknown"), null);
  assert.equal(viewForPath("/settings/extra"), null);
  assert.equal(viewForPath(""), "devices");
  assert.equal(viewForPath(null), "devices");
});

test("pages open to the permissions that use them", () => {
  const technician = account(["devices.view", "sessions.connect", "scripts.run"]);
  assert.equal(canView(technician, "devices"), true);
  assert.equal(canView(technician, "toolbox"), true);
  for (const view of ["users", "roles", "authentication", "settings", "audit"]) assert.equal(canView(technician, view), false, view);
  assert.equal(canView(technician, "account"), true);
  assert.equal(canView(account(["files.manage_shared"]), "toolbox"), true);
  assert.equal(canView(account(["audit.view"]), "audit"), true);
  // Email settings are for administrators only, on the Authentication page.
  assert.equal(canView(account([], true), "authentication"), true);
  assert.equal(canView(null, "account"), false);
});

test("users land on Devices, or their first page without it", () => {
  assert.equal(homeView(account(["devices.view"])), "devices");
  assert.equal(homeView(account(["users.manage", "audit.view"])), "users");
  assert.equal(homeView(account([])), "account");
});
