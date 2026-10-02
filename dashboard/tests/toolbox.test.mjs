import assert from "node:assert/strict";
import test from "node:test";
import {
  emptyScriptDraft,
  folderSuggestions,
  formatBytes,
  groupByFolder,
  isValidFileName,
  matchesQuery,
  normalizeFolder,
  ranAsLabel,
  runOutcome,
  runTone,
  scriptBody,
  scriptDraftProblem,
  uploadProblem,
} from "../features/toolbox/model.ts";

test("folders are normalized like the server stores them", () => {
  assert.equal(normalizeFolder(""), "");
  assert.equal(normalizeFolder(" Installers / Chrome "), "Installers/Chrome");
  assert.equal(normalizeFolder("Disk\\Cleanup//"), "Disk/Cleanup");
  assert.equal(normalizeFolder("a/".repeat(9)), null);
  assert.equal(normalizeFolder("x".repeat(65)), null);
  assert.equal(normalizeFolder(Array(4).fill("y".repeat(64)).join("/")), null, "the whole path is bounded");
});

test("file names must be ones Windows can create", () => {
  for (const name of ["setup.exe", "Read me.txt", "Zoë 王.pdf", "console.txt"]) assert.ok(isValidFileName(name), name);
  for (const name of ["", "a/b", "what?.txt", "trailing.", "trailing ", " leading", "CON", "con.txt", "LPT9.log", "x".repeat(256)]) {
    assert.ok(!isValidFileName(name), name);
  }
  assert.equal(uploadProblem({ name: "setup.exe", size: 95 * 1024 * 1024 }), null);
  assert.match(uploadProblem({ name: "setup.exe", size: 95 * 1024 * 1024 + 1 }), /95 MiB/);
  assert.match(uploadProblem({ name: "nul", size: 1 }), /file name/);
});

test("script drafts explain what keeps them from saving", () => {
  const draft = { ...emptyScriptDraft("Maintenance"), name: " Restart spooler ", body: "Restart-Service Spooler" };
  assert.equal(scriptDraftProblem(draft), null);
  assert.deepEqual(scriptBody({ ...draft, folder: " Maintenance / Printers " }), {
    name: "Restart spooler", folder: "Maintenance/Printers", description: "", language: "powershell",
    body: "Restart-Service Spooler", timeout_seconds: 300, shared: false,
  });
  assert.match(scriptDraftProblem({ ...draft, name: "  " }), /name/);
  assert.match(scriptDraftProblem({ ...draft, body: " \n " }), /empty/);
  assert.match(scriptDraftProblem({ ...draft, body: "é".repeat(64 * 1024 + 1) }), /128 KiB/, "the limit is in UTF-8 bytes");
  assert.match(scriptDraftProblem({ ...draft, timeoutSeconds: "9" }), /timeout/);
  assert.match(scriptDraftProblem({ ...draft, timeoutSeconds: "60.5" }), /timeout/);
  assert.match(scriptDraftProblem({ ...draft, folder: "a/".repeat(9) }), /Folders/);
});

test("items group by folder with the top level first", () => {
  const items = [
    { folder: "Zeta", name: "b" },
    { folder: "", name: "Top" },
    { folder: "Alpha/Inner", name: "z" },
    { folder: "Alpha/Inner", name: "a" },
  ];
  assert.deepEqual(groupByFolder(items).map(({ folder, items }) => [folder, items.map((item) => item.name)]), [
    ["", ["Top"]],
    ["Alpha/Inner", ["a", "z"]],
    ["Zeta", ["b"]],
  ]);
  assert.deepEqual(folderSuggestions(items), ["Alpha", "Alpha/Inner", "Zeta"]);
});

test("search matches names, folders and descriptions", () => {
  const script = { name: "Restart spooler", folder: "Printers", description: "Clears stuck jobs" };
  assert.ok(matchesQuery(script, ""));
  assert.ok(matchesQuery(script, "SPOOL"));
  assert.ok(matchesQuery(script, "printers"));
  assert.ok(matchesQuery(script, "stuck"));
  assert.ok(!matchesQuery(script, "disk"));
});

test("runs read as their outcome and the account they used", () => {
  assert.equal(runOutcome({ status: "pending" }), "Running…");
  assert.equal(runOutcome({ status: "completed", exit_code: 0 }), "Exit code 0");
  assert.equal(runOutcome({ status: "timed_out", exit_code: 1 }), "Timed out");
  assert.equal(runOutcome({ status: "lost" }), "No result");
  assert.equal(runTone({ status: "completed", exit_code: 0 }), "success");
  assert.equal(runTone({ status: "completed", exit_code: 2 }), "problem");
  assert.equal(runTone({ status: "pending" }), "pending");
  assert.equal(ranAsLabel({ run_as: "user" }), "Signed-in user");
  assert.equal(ranAsLabel({ run_as: "user", ran_as: "PC\\ada" }), "PC\\ada");
  assert.equal(ranAsLabel({ run_as: "user", ran_as: "NT AUTHORITY\\SYSTEM" }), "NT AUTHORITY\\SYSTEM (nobody was signed in)");
  assert.equal(ranAsLabel({ run_as: "system", ran_as: "NT AUTHORITY\\SYSTEM" }), "NT AUTHORITY\\SYSTEM");
});

test("sizes read in binary units", () => {
  assert.equal(formatBytes(0), "0 B");
  assert.equal(formatBytes(1536), "1.5 KiB");
  assert.equal(formatBytes(95 * 1024 * 1024), "95 MiB");
});
