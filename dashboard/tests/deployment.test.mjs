import assert from "node:assert/strict";
import test from "node:test";
import { canRevoke, expiryText, lastUsedText, newTokenProblem, parseLimit, revoked, usesText } from "../features/deployment/model.ts";
import { linuxInstallCommand, macInstallCommand, windowsQuietCommand } from "../features/enrollment/installer.ts";
import { canView, navViewFor, viewForPath } from "../features/workspace/views.ts";

const DAY = 24 * 60 * 60_000;
const NOW = 1_800_000_000_000;

const token = (fields = {}) => ({
  id: "t1",
  name: "Rollout",
  company_id: "c1",
  company_name: "Acme",
  created_by: { id: "u1", email: "admin@example.com" },
  created_at: NOW - DAY,
  expires_at: NOW + 3 * DAY,
  max_uses: null,
  uses: 0,
  last_used_at: null,
  revoked_at: null,
  state: "active",
  ...fields,
});

test("the use limit is a whole number in range, or empty for none", () => {
  assert.equal(parseLimit(""), null);
  assert.equal(parseLimit("  "), null);
  assert.equal(parseLimit("1"), 1);
  assert.equal(parseLimit(" 250 "), 250);
  assert.equal(parseLimit("1000000"), 1_000_000);
  for (const invalid of ["0", "-3", "2.5", "1e3", "1000001", "ten"]) assert.equal(parseLimit(invalid), undefined, invalid);
});

test("a new token needs a name, a company and a valid limit", () => {
  assert.match(newTokenProblem("  ", "c1", ""), /Name/);
  assert.match(newTokenProblem("x".repeat(81), "c1", ""), /80/);
  assert.equal(newTokenProblem("x".repeat(80), "c1", ""), null);
  assert.match(newTokenProblem("Rollout", "", ""), /company/);
  assert.match(newTokenProblem("Rollout", "c1", "0"), /whole number/);
  assert.equal(newTokenProblem("Rollout", "c1", "50"), null);
});

test("uses read with or without a limit", () => {
  assert.equal(usesText(token()), "No devices yet · no limit");
  assert.equal(usesText(token({ uses: 1 })), "1 device · no limit");
  assert.equal(usesText(token({ uses: 12 })), "12 devices · no limit");
  assert.equal(usesText(token({ uses: 3, max_uses: 10 })), "3 of 10 devices");
  assert.equal(usesText(token({ uses: 1, max_uses: 1 })), "1 of 1 device");
});

test("expiry and last use read relative to now, and revocation wins", () => {
  assert.equal(expiryText(token(), NOW), "Expires in 3 days");
  assert.equal(expiryText(token({ expires_at: NOW - 2 * DAY }), NOW), "Expired 2 days ago");
  assert.equal(expiryText(token({ revoked_at: NOW - DAY }), NOW), "Revoked yesterday");
  assert.equal(lastUsedText(token(), NOW), "Never used");
  assert.equal(lastUsedText(token({ last_used_at: NOW - 2 * DAY }), NOW), "Last enrolled a device 2 days ago");
});

test("only tokens that still enroll can be revoked, and revoking marks one revoked", () => {
  assert.ok(canRevoke(token()));
  assert.ok(canRevoke(token({ state: "exhausted" })));
  assert.ok(!canRevoke(token({ state: "expired" })));
  assert.ok(!canRevoke(token({ state: "revoked" })));
  const after = revoked(token(), NOW);
  assert.equal(after.state, "revoked");
  assert.equal(after.revoked_at, NOW);
  assert.equal(revoked(token({ revoked_at: 5 }), NOW).revoked_at, 5);
});

test("mass deployment runs the Windows installer quietly and the Unix commands as root", () => {
  assert.equal(windowsQuietCommand("windows-x64"), "MeshRMM-Agent-Setup-Windows-x64.exe --quiet");
  assert.equal(windowsQuietCommand("windows-arm64"), "MeshRMM-Agent-Setup-Windows-ARM64.exe --quiet");
  const bootstrap = { server: "https://rmm.example.com", install_token: "ab", expires_at_unix_ms: 1 };
  const authorization = Buffer.from(JSON.stringify(bootstrap)).toString("hex");
  assert.equal(
    linuxInstallCommand("https://rmm.example.com/", bootstrap, { sudo: false }),
    `curl -fsSL https://rmm.example.com/install-agent-linux.sh | sh -s -- https://rmm.example.com ${authorization}`,
  );
  assert.equal(
    macInstallCommand("https://rmm.example.com", bootstrap, { sudo: false }),
    `curl -fsSL https://rmm.example.com/install-agent-macos.sh | /bin/sh -s -- https://rmm.example.com ${authorization}`,
  );
  // The single-use dialog keeps sudo, for a person at a terminal.
  assert.match(linuxInstallCommand("https://rmm.example.com", bootstrap), /\| sudo sh -s -- /);
  assert.match(macInstallCommand("https://rmm.example.com", bootstrap), /\| sudo \/bin\/sh -s -- /);
});

test("the deployment tokens page opens to devices.enroll and sits under Companies", () => {
  assert.equal(viewForPath("/deployment"), "deployment");
  assert.equal(navViewFor("deployment"), "companies");
  const account = (permissions) => ({ permissions, is_administrator: false });
  assert.equal(canView(account(["devices.enroll"]), "deployment"), true);
  assert.equal(canView(account(["devices.view"]), "deployment"), false);
});
