import assert from "node:assert/strict";
import { createHash } from "node:crypto";
import test from "node:test";
import { Sha256, sha256OfBlob } from "../lib/sha256.ts";
import {
  PAGE_SIZE,
  ROW_HEIGHT,
  accountLabel,
  accountOptions,
  baseName,
  crumbs,
  errorHelp,
  filesPagePath,
  isRoot,
  joinPath,
  joinRelative,
  nameProblem,
  pagesFor,
  parentPath,
  stemLength,
  trashName,
  typeLabel,
  visibleRows,
} from "../features/files/model.ts";
import { emptyFolders, fromFileList } from "../features/files/picked.ts";

test("Windows paths split into drives, shares and folders", () => {
  assert.equal(isRoot("windows", "C:\\"), true);
  assert.equal(isRoot("windows", "C:"), true);
  assert.equal(isRoot("windows", "\\\\nas\\share"), true);
  assert.equal(isRoot("windows", "C:\\Users"), false);
  assert.equal(parentPath("windows", "C:\\Users\\ada"), "C:\\Users");
  assert.equal(parentPath("windows", "C:\\Users"), "C:\\");
  assert.equal(parentPath("windows", "C:\\"), null);
  assert.equal(parentPath("windows", "\\\\nas\\share\\docs"), "\\\\nas\\share");
  assert.equal(joinPath("windows", "C:\\", "Windows"), "C:\\Windows");
  assert.equal(joinPath("windows", "C:\\Users", "ada"), "C:\\Users\\ada");
  assert.equal(joinRelative("windows", "D:\\in", "Photos/2026/beach.jpg"), "D:\\in\\Photos\\2026\\beach.jpg");
  assert.equal(baseName("windows", "C:\\Users\\ada"), "ada");
  assert.deepEqual(crumbs("windows", "C:\\Users\\ada"), [
    { label: "C:", path: "C:\\" },
    { label: "Users", path: "C:\\Users" },
    { label: "ada", path: "C:\\Users\\ada" },
  ]);
});

test("macOS and Linux paths split from /", () => {
  assert.equal(isRoot("linux", "/"), true);
  assert.equal(parentPath("linux", "/home/ada"), "/home");
  assert.equal(parentPath("macos", "/Users"), "/");
  assert.equal(parentPath("linux", "/"), null);
  assert.equal(joinPath("linux", "/", "etc"), "/etc");
  assert.equal(joinPath("linux", "/etc", "hosts"), "/etc/hosts");
  assert.equal(joinRelative("macos", "/tmp", "a/../b/./c.txt"), "/tmp/a/b/c.txt");
  assert.deepEqual(crumbs("linux", "/var/log"), [
    { label: "/", path: "/" },
    { label: "var", path: "/var" },
    { label: "log", path: "/var/log" },
  ]);
  assert.equal(filesPagePath("d1", "/var/log", "user"), "/files?id=d1&path=%2Fvar%2Flog&as=user");
  assert.equal(filesPagePath("d1"), "/files?id=d1");
});

test("entries are described and named the way each platform allows", () => {
  assert.equal(typeLabel({ name: "notes.txt", kind: "file" }), "TXT file");
  assert.equal(typeLabel({ name: "Makefile", kind: "file" }), "File");
  assert.equal(typeLabel({ name: ".profile", kind: "file" }), "File");
  assert.equal(typeLabel({ name: "docs", kind: "directory" }), "Folder");
  assert.equal(typeLabel({ name: "docs", kind: "directory", link: true }), "Link to folder");
  assert.equal(typeLabel({ name: "a.pdf", kind: "file", link: true }), "Link to PDF file");
  assert.equal(typeLabel({ name: "gone", kind: "other", link: true }), "Broken link");
  assert.equal(stemLength("report.final.pdf", "file"), 12);
  assert.equal(stemLength("archive.d", "directory"), 9);
  assert.equal(nameProblem("windows", "a:b"), "Windows doesn’t allow < > : \" | ? * in names.");
  assert.equal(nameProblem("linux", "a:b"), null);
  assert.ok(nameProblem("windows", "CON.txt"));
  assert.ok(nameProblem("windows", "trailing."));
  assert.equal(nameProblem("macos", "trailing."), null);
  assert.ok(nameProblem("linux", "a/b"));
  assert.ok(nameProblem("linux", ".."));
  assert.ok(nameProblem("linux", "  "));
});

test("accounts follow the permissions, and errors say the way on", () => {
  const can = (held) => (permission) => held.includes(permission);
  assert.deepEqual(accountOptions(can(["files.browse", "files.system"])), ["system", "user"]);
  assert.deepEqual(accountOptions(can(["files.browse"])), ["user"]);
  assert.deepEqual(accountOptions(can(["files.system"])), []);
  assert.equal(accountLabel("system", "windows"), "SYSTEM");
  assert.equal(accountLabel("system", "linux"), "root");
  assert.equal(trashName("windows"), "Recycle Bin");
  assert.equal(trashName("macos"), "Trash");
  assert.match(errorHelp("access_denied", "user", "linux"), /root/);
  assert.match(errorHelp("access_denied", "system", "macos"), /Full Disk Access/);
  assert.equal(errorHelp("access_denied", "system", "windows"), null);
  assert.match(errorHelp("no_user", "user", "windows"), /SYSTEM/);
});

test("only the rows on screen are drawn, and their pages loaded", () => {
  assert.deepEqual(visibleRows(0, ROW_HEIGHT * 20, 50_000), { first: 0, last: 30 });
  assert.deepEqual(visibleRows(ROW_HEIGHT * 1000, ROW_HEIGHT * 20, 50_000), { first: 990, last: 1030 });
  assert.deepEqual(visibleRows(ROW_HEIGHT * 49_990, ROW_HEIGHT * 20, 50_000), { first: 49_980, last: 50_000 });
  assert.deepEqual(visibleRows(0, 600, 0), { first: 0, last: 0 });
  assert.deepEqual(pagesFor(0, 30), [0]);
  assert.deepEqual(pagesFor(PAGE_SIZE - 5, PAGE_SIZE + 5), [0, 1]);
  assert.deepEqual(pagesFor(49_980, 50_000), [99]);
});

test("folder uploads keep their paths and make their empty folders", () => {
  const file = (name, webkitRelativePath = "") => ({ name, webkitRelativePath });
  assert.deepEqual(
    fromFileList([file("a.txt"), file("b.txt", "Photos/2026/b.txt")]).files.map((picked) => picked.relative),
    ["a.txt", "Photos/2026/b.txt"],
  );
  const picked = {
    files: [{ file: file("x"), relative: "top/sub/x" }],
    folders: ["top", "top/sub", "top/empty", "top/empty/deeper", "alone"],
  };
  assert.deepEqual(emptyFolders(picked), ["top/empty/deeper", "alone"]);
});

test("SHA-256 matches node's, a piece at a time", async () => {
  for (const size of [0, 3, 55, 56, 64, 65, 1000, 300_001]) {
    const data = new Uint8Array(size).map((_, index) => (index * 7 + 3) & 255);
    const hash = new Sha256();
    for (let start = 0, step = 1; start < size; start += step, step = step * 2 + 1) hash.update(data.subarray(start, start + step));
    assert.equal(hash.hex(), createHash("sha256").update(data).digest("hex"), `${size} bytes`);
  }
  const blob = new Blob([new Uint8Array(9 * 1024 * 1024).fill(5)]);
  const seen = [];
  const digest = await sha256OfBlob(blob, (bytes) => seen.push(bytes));
  assert.equal(digest, createHash("sha256").update(new Uint8Array(9 * 1024 * 1024).fill(5)).digest("hex"));
  assert.deepEqual(seen, [4 * 1024 * 1024, 8 * 1024 * 1024, 9 * 1024 * 1024]);
});
