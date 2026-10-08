import assert from "node:assert/strict";
import test from "node:test";
import { defaultPasskeyName, twoFactorSummary } from "../features/account/model.ts";
import { actionLabel, eventLabel, targetLabel } from "../features/audit/model.ts";
import { ssoForm, ssoFormProblem, ssoUpdate } from "../features/authentication/model.ts";
import { groupPermissions } from "../features/roles/permissions.ts";
import { toolboxAccess } from "../features/toolbox/access.ts";
import { groupRoleLabel, matchesUser, sortRoles, sortUsers } from "../features/users/model.ts";
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

test("the user search matches roles from identity provider groups", () => {
  const user = { display_name: "Ada", email: "ada@example.com", roles: [], group_roles: [{ id: "t", name: "Technician", source: "scim", group: "IT" }] };
  assert.equal(matchesUser(user, "techn"), true);
  assert.equal(matchesUser(user, "admin"), false);
});

test("roles from groups name their group and where it comes from", () => {
  const scim = { name: "Technician", source: "scim", group: "IT" };
  assert.equal(groupRoleLabel(scim, "Okta"), "Technician · via SCIM group “IT”");
  const sso = { name: "Administrator", source: "sso", group: "admins" };
  assert.equal(groupRoleLabel(sso, "Okta"), "Administrator · via Okta group “admins”");
  assert.equal(groupRoleLabel(sso, null), "Administrator · via SSO group “admins”");
});

test("new audit events read as sentences, with how sign-ins happened", () => {
  for (const action of [
    "account.passkey_add", "account.passkey_rename", "account.passkey_remove",
    "user.sso_link", "user.sso_unlink", "user.provision",
    "settings.sso_update", "settings.sso_delete",
    "scim.token_create", "scim.token_revoke", "scim.group_role_update",
    "scim.user_create", "scim.user_update", "scim.user_delete",
    "scim.group_create", "scim.group_update", "scim.group_delete",
  ]) assert.notEqual(actionLabel(action), action, action);
  assert.equal(eventLabel({ action: "auth.sign_in", metadata: { method: "passkey" } }), "Signed in with a passkey");
  assert.equal(eventLabel({ action: "auth.sign_in", metadata: { method: "oidc", groups: [] } }), "Signed in with SSO");
  assert.equal(eventLabel({ action: "auth.sign_in", metadata: { method: "password", second_factor: null } }), "Signed in with a password");
  assert.equal(eventLabel({ action: "auth.sign_in", metadata: {} }), "Signed in");
  assert.equal(eventLabel({ action: "auth.sign_in_failed", metadata: { reason: "passkey" } }), "Failed to sign in: passkey not accepted");
  assert.equal(eventLabel({ action: "auth.sign_in_failed", metadata: { reason: "account_disabled", method: "oidc" } }), "Failed to sign in: account disabled");
  assert.equal(eventLabel({ action: "auth.sign_in_failed", metadata: { reason: "future" } }), "Failed to sign in");
  assert.equal(eventLabel({ action: "user.provision", metadata: { source: "sso", email: "a@x" } }), "Created an account at first sign-in with SSO");
  assert.equal(targetLabel({ actor_user_id: null, target_type: "scim_group", target_id: "g1", metadata: { display_name: "IT" } }), "SCIM group: IT");
  assert.equal(targetLabel({ actor_user_id: null, target_type: "scim_token", target_id: "t1", metadata: { name: "Okta" } }), "SCIM token: Okta");
  assert.equal(targetLabel({ actor_user_id: null, target_type: "scim_token", target_id: "t1", metadata: {} }), "SCIM token: t1");
  // SCIM actors have no user ID, so no target is their own account.
  assert.equal(targetLabel({ actor_user_id: null, target_type: "user", target_id: "u1", metadata: { email: "a@x", display_name: "A" } }), "User: a@x");
});

test("the account page sums up its second factors", () => {
  assert.equal(twoFactorSummary({ enabled: false, totp: false, passkeys: 0 }), "Off. Signing in takes only your password.");
  assert.equal(twoFactorSummary({ enabled: true, totp: true, passkeys: 0 }), "On, with an authenticator app.");
  assert.equal(twoFactorSummary({ enabled: true, totp: false, passkeys: 1 }), "On, with 1 passkey.");
  assert.equal(twoFactorSummary({ enabled: true, totp: true, passkeys: 3 }), "On, with an authenticator app and 3 passkeys.");
});

test("new passkeys are named after the device they're made on", () => {
  assert.equal(defaultPasskeyName("Mozilla/5.0 (Macintosh; Intel Mac OS X 15_6) AppleWebKit/605.1.15 (KHTML, like Gecko) Version/26.0 Safari/605.1.15"), "Mac passkey");
  assert.equal(defaultPasskeyName("Mozilla/5.0 (iPhone; CPU iPhone OS 18_0 like Mac OS X) AppleWebKit/605.1.15"), "iPhone passkey");
  assert.equal(defaultPasskeyName("Mozilla/5.0 (Windows NT 10.0; Win64; x64) Chrome/140.0"), "Windows passkey");
  assert.equal(defaultPasskeyName("Mozilla/5.0 (Linux; Android 15) Chrome/140.0 Mobile"), "Android passkey");
  assert.equal(defaultPasskeyName(""), "Passkey");
});

test("the single sign-on form keeps, replaces or removes the client secret", () => {
  const provider = {
    enabled: true,
    display_name: "Okta",
    issuer_url: "https://example.okta.com",
    client_id: "abc",
    has_client_secret: true,
    scopes: "openid email profile",
    auto_provision: true,
    default_role_id: "technician",
    require_verified_email: true,
    groups_claim: "groups",
    group_roles: [{ group: "IT", role_id: "technician" }],
    updated_at: 1,
  };
  const form = ssoForm(provider);
  assert.equal(ssoFormProblem(form), null);
  const kept = ssoUpdate(form);
  assert.equal("client_secret" in kept, false);
  assert.deepEqual(kept, {
    enabled: true,
    display_name: "Okta",
    issuer_url: "https://example.okta.com",
    client_id: "abc",
    scopes: "openid email profile",
    auto_provision: true,
    default_role_id: "technician",
    require_verified_email: true,
    groups_claim: "groups",
    group_roles: [{ group: "IT", role_id: "technician" }],
  });
  assert.equal(ssoUpdate({ ...form, clientSecret: "s3cret" }).client_secret, "s3cret");
  assert.equal(ssoUpdate({ ...form, removeSecret: true }).client_secret, null);
  // A typed secret wins over removing the stored one.
  assert.equal(ssoUpdate({ ...form, clientSecret: "new", removeSecret: true }).client_secret, "new");
});

test("the single sign-on form trims, defaults and checks its fields", () => {
  const blank = ssoForm(null);
  assert.equal(blank.enabled, true);
  assert.equal(blank.requireVerifiedEmail, true);
  assert.equal(blank.scopes, "openid email profile");
  assert.match(ssoFormProblem(blank), /name/);
  const form = { ...blank, displayName: " Entra ID ", issuerUrl: " https://login.microsoftonline.com/t/v2.0 ", clientId: " id ", scopes: "  ", groupsClaim: "  " , groupRoles: [{ group: "x", role_id: "" }] };
  // Without a groups claim, mappings are dropped and not checked.
  assert.equal(ssoFormProblem(form), null);
  const body = ssoUpdate(form);
  assert.equal(body.display_name, "Entra ID");
  assert.equal(body.issuer_url, "https://login.microsoftonline.com/t/v2.0");
  assert.equal(body.client_id, "id");
  assert.equal(body.scopes, "openid email profile");
  assert.equal(body.groups_claim, null);
  assert.deepEqual(body.group_roles, []);
  assert.equal(body.default_role_id, null);
  const grouped = { ...form, groupsClaim: "realm_access.roles" };
  assert.match(ssoFormProblem(grouped), /role/);
  assert.match(ssoFormProblem({ ...grouped, groupRoles: [{ group: " ", role_id: "t" }] }), /group name/);
  assert.deepEqual(ssoUpdate({ ...grouped, groupRoles: [{ group: " IT ", role_id: "t" }] }).group_roles, [{ group: "IT", role_id: "t" }]);
  assert.match(ssoFormProblem({ ...form, displayName: "x".repeat(81) }), /80/);
});
