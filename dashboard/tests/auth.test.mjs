import assert from "node:assert/strict";
import test from "node:test";
import { loginPath, safeNextPath, tokenFromHash } from "../features/auth/next-path.ts";
import { newPasswordProblem } from "../features/auth/passwords.ts";
import { firstSecondFactor, signInMethodLabel, ssoErrorMessage, ssoStartPath } from "../features/auth/sign-in.ts";
import { asSentence } from "../lib/http.ts";

test("after sign-in, only paths on this site are followed", () => {
  assert.equal(safeNextPath("/toolbox"), "/toolbox");
  assert.equal(safeNextPath("/?q=pc&status=online"), "/?q=pc&status=online");
  for (const unsafe of [null, "", "toolbox", "//evil.example/x", "/\\evil.example", "https://evil.example/", "javascript:alert(1)"]) {
    assert.equal(safeNextPath(unsafe), "/", String(unsafe));
  }
  // Returning to the sign-in page would only ask again.
  assert.equal(safeNextPath("/login?next=/users"), "/");
});

test("the sign-in page carries where to return", () => {
  assert.equal(loginPath("/"), "/login");
  assert.equal(loginPath("/users"), "/login?next=%2Fusers");
  assert.equal(loginPath("//evil.example"), "/login");
});

test("one-time links carry their token in the fragment", () => {
  assert.equal(tokenFromHash("#token=abc123"), "abc123");
  assert.equal(tokenFromHash("token=abc123"), "abc123");
  assert.equal(tokenFromHash(""), null);
  assert.equal(tokenFromHash("#token="), null);
  assert.equal(tokenFromHash("#other=1"), null);
});

test("new passwords follow the server's rules", () => {
  assert.equal(newPasswordProblem("correct horse battery", "correct horse battery", 12), null);
  assert.match(newPasswordProblem("short", "short", 12), /at least 12/);
  // Characters count, not UTF-16 units: four emoji are four characters.
  assert.match(newPasswordProblem("😀😀😀😀", "😀😀😀😀", 5), /at least 5/);
  assert.equal(newPasswordProblem("😀😀😀😀😀", "😀😀😀😀😀", 5), null);
  assert.match(newPasswordProblem(" ".repeat(12), " ".repeat(12), 12), /only spaces/);
  assert.match(newPasswordProblem("é".repeat(513), "é".repeat(513), 12), /shorter/);
  assert.match(newPasswordProblem("correct horse battery", "correct horse", 12), /don't match/);
});

test("server errors read as sentences", () => {
  assert.equal(asSentence("the code is incorrect"), "The code is incorrect.");
  assert.equal(asSentence("Device not found"), "Device not found.");
  assert.equal(asSentence("too many attempts; wait a few minutes and try again"), "Too many attempts; wait a few minutes and try again.");
  assert.equal(asSentence("Done!"), "Done!");
  assert.equal(asSentence("  "), "");
});

test("single sign-on starts as a page load that returns to a safe path", () => {
  assert.equal(ssoStartPath("/users?q=ada"), "/v1/auth/sso/start?next=%2Fusers%3Fq%3Dada");
  assert.equal(ssoStartPath(null), "/v1/auth/sso/start?next=%2F");
  assert.equal(ssoStartPath("//evil.example"), "/v1/auth/sso/start?next=%2F");
  assert.equal(ssoStartPath("https://evil.example/"), "/v1/auth/sso/start?next=%2F");
});

test("every single sign-on failure has its own message, and unknown ones read as failed", () => {
  const codes = ["unavailable", "failed", "expired", "denied", "no_account", "no_email", "email_unverified", "conflict", "account_disabled", "rate_limited"];
  const messages = codes.map(ssoErrorMessage);
  assert.equal(new Set(messages).size, codes.length);
  for (const message of messages) assert.match(message, /^[A-Z].*\.$/);
  assert.match(ssoErrorMessage("no_account"), /invite/);
  assert.match(ssoErrorMessage("conflict"), /unlink/);
  assert.equal(ssoErrorMessage("something_new"), ssoErrorMessage("failed"));
  assert.equal(ssoErrorMessage(null), null);
  assert.equal(ssoErrorMessage(""), null);
});

test("the second step starts with the authenticator app, else a usable passkey", () => {
  assert.equal(firstSecondFactor(["totp", "passkey", "recovery_code"], true), "totp");
  assert.equal(firstSecondFactor(["passkey", "recovery_code"], true), "passkey");
  // Without passkey support in this browser, or once the prompt is spent.
  assert.equal(firstSecondFactor(["passkey", "recovery_code"], false), "recovery_code");
  assert.equal(firstSecondFactor(["recovery_code"], true), "recovery_code");
});

test("sessions name how they signed in", () => {
  assert.equal(signInMethodLabel("password"), "Password");
  assert.equal(signInMethodLabel("passkey"), "Passkey");
  assert.equal(signInMethodLabel("oidc"), "SSO");
  assert.equal(signInMethodLabel("future"), "future");
});
