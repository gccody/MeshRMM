import assert from "node:assert/strict";
import test from "node:test";
import { actionLabel, targetLabel } from "../features/audit/model.ts";
import { groupPermissions } from "../features/roles/permissions.ts";
import { toolboxAccess } from "../features/toolbox/access.ts";
import { matchesUser, sortRoles, sortUsers } from "../features/users/model.ts";
import { describeUserAgent, formatRelative } from "../lib/format.ts";

const holding = (...permissions) => (permission) => permissions.includes(permission);

test("the toolbox follows the use and manage permissions of each kind", () => {
  const runner = toolboxAccess(holding("scripts.run"));
  assert.deepEqual(runner.tabs, ["scripts", "runs"]);
  assert.equal(runner.addScripts, true);
  assert.equal(runner.shareScripts, false);
  assert.equal(runner.addFiles, false);

  // Managing shared scripts alone means every new script is shared.
  const librarian = toolboxAccess(holding("scripts.manage_shared", "files.manage_shared"));
  assert.deepEqual(librarian.tabs, ["scripts", "files"]);
  assert.equal(librarian.runScripts, false);
  assert.equal(librarian.keepPrivateScripts, false);
  assert.equal(librarian.shareFiles, true);
  assert.equal(librarian.keepPrivateFiles, false);

  assert.deepEqual(toolboxAccess(holding("audit.view")).tabs, ["runs"]);
  assert.deepEqual(toolboxAccess(holding()).tabs, []);
});

test("permissions group by what they govern, in the server's order", () => {
  const groups = groupPermissions([
    { name: "devices.view", description: "" },
    { name: "devices.enroll", description: "" },
    { name: "sessions.connect", description: "" },
    { name: "audit.view", description: "" },
  ]);
  assert.deepEqual(groups.map(({ label, items }) => [label, items.map((item) => item.name)]), [
    ["Devices", ["devices.view", "devices.enroll"]],
    ["Remote sessions", ["sessions.connect"]],
    ["Audit log", ["audit.view"]],
  ]);
});

test("built-in roles come first, then roles and users by name", () => {
  const roles = sortRoles([
    { name: "zeta", builtin: null },
    { name: "Technician", builtin: "technician" },
    { name: "alpha", builtin: null },
    { name: "Administrator", builtin: "administrator" },
  ]);
  assert.deepEqual(roles.map((role) => role.name), ["Administrator", "Technician", "alpha", "zeta"]);
  const users = sortUsers([{ display_name: "bo", email: "b@x" }, { display_name: "Al", email: "a@x" }]);
  assert.deepEqual(users.map((user) => user.display_name), ["Al", "bo"]);
});

test("the user search matches names, emails and roles", () => {
  const user = { display_name: "Ada Lovelace", email: "ada@example.com", roles: [{ id: "t", name: "Technician" }] };
  for (const query of ["", "ada", "EXAMPLE", "techn"]) assert.equal(matchesUser(user, query), true, query);
  assert.equal(matchesUser(user, "grace"), false);
});

test("audit events read as sentences, naming their target", () => {
  assert.equal(actionLabel("auth.sign_in"), "Signed in");
  assert.equal(actionLabel("future.action"), "future.action");
  assert.equal(targetLabel({ target_type: "agent", target_id: "d1", metadata: { name: "FRONT-DESK" } }), "Device: FRONT-DESK");
  assert.equal(targetLabel({ target_type: "invitation", target_id: "i1", metadata: { email: "tess@example.com" } }), "Invitation: tess@example.com");
  assert.equal(targetLabel({ actor_user_id: "u2", target_type: "user", target_id: "u1", metadata: {} }), "User: u1");
  assert.equal(targetLabel({ actor_user_id: "u1", target_type: "user", target_id: "u1", metadata: {} }), "Own account");
  assert.equal(targetLabel({ target_type: "settings", target_id: "instance", metadata: { name: "x" } }), "Settings");
});

test("sessions are described by browser and system", () => {
  const cases = [
    ["Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/140.0 Safari/537.36 Edg/140.0", "Edge on Windows"],
    ["Mozilla/5.0 (Macintosh; Intel Mac OS X 15_6) AppleWebKit/605.1.15 (KHTML, like Gecko) Version/26.0 Safari/605.1.15", "Safari on macOS"],
    ["Mozilla/5.0 (X11; Linux x86_64; rv:140.0) Gecko/20100101 Firefox/140.0", "Firefox on Linux"],
    ["Mozilla/5.0 (iPhone; CPU iPhone OS 18_0 like Mac OS X) AppleWebKit/605.1.15 (KHTML, like Gecko) CriOS/140.0 Mobile/15E148 Safari/604.1", "Chrome on iOS"],
    ["curl/8.7.1", "Unknown browser"],
    [null, "Unknown browser"],
  ];
  for (const [agent, expected] of cases) assert.equal(describeUserAgent(agent), expected, String(agent));
});

test("times read relative to now", () => {
  const now = 1_700_000_000_000;
  assert.equal(formatRelative(now - 30_000, now), "just now");
  assert.match(formatRelative(now - 5 * 60_000, now), /5 minutes ago/);
  assert.match(formatRelative(now + 3 * 24 * 60 * 60_000, now), /in 3 days/);
});
