import assert from "node:assert/strict";
import test from "node:test";
import {
  MAX_REGISTRY_VALUE_BYTES,
  childPath,
  dataFromDraft,
  draftOf,
  editable,
  formatData,
  freshName,
  keyDisplay,
  keyNameProblem,
  parentPath,
  parseKeyPath,
  rebase,
  sortedValues,
  storedBytes,
  typeName,
} from "../features/tools/registry.ts";
import { unavailableReason } from "../features/tools/model.ts";

test("keys are named and pasted as Regedit names them", () => {
  assert.deepEqual(parseKeyPath("HKLM\\SOFTWARE\\Contoso"), { root: "HKLM", path: "SOFTWARE\\Contoso" });
  assert.deepEqual(parseKeyPath("Computer\\HKEY_LOCAL_MACHINE\\SOFTWARE\\"), { root: "HKLM", path: "SOFTWARE" });
  assert.deepEqual(parseKeyPath("  hkey_users/S-1-5-18 "), { root: "HKU", path: "S-1-5-18" });
  assert.deepEqual(parseKeyPath("HKCC"), { root: "HKCC", path: "" });
  // HKEY_CURRENT_USER is each user's, under HKEY_USERS.
  assert.equal(parseKeyPath("HKCU\\Software"), null);
  assert.equal(parseKeyPath("SOFTWARE"), null);
  assert.equal(keyDisplay({ root: "HKCR", path: ".txt" }), "HKEY_CLASSES_ROOT\\.txt");
  assert.equal(keyDisplay({ root: "HKLM", path: "" }), "HKEY_LOCAL_MACHINE");
  assert.equal(childPath("", "SOFTWARE"), "SOFTWARE");
  assert.equal(childPath("SOFTWARE", "Contoso"), "SOFTWARE\\Contoso");
  assert.equal(parentPath("SOFTWARE\\Contoso"), "SOFTWARE");
  assert.equal(parentPath("SOFTWARE"), "");
});

test("new keys' names follow Windows' rules", () => {
  assert.equal(keyNameProblem("Contoso"), null);
  assert.match(keyNameProblem(""), /name/);
  assert.match(keyNameProblem("a\\b"), /backslash/);
  assert.match(keyNameProblem("x".repeat(256)), /255/);
  assert.match(keyNameProblem("a\u0001"), /control/);
  assert.equal(freshName("New Key", ["new key #1", "New Key #2"]), "New Key #3");
});

test("data reads as Regedit shows it", () => {
  const value = (data, extra = {}) => ({ name: "v", size: 4, ...data, ...extra });
  assert.equal(formatData(value({ type: "dword", data: 42 })), "0x0000002a (42)");
  assert.equal(formatData(value({ type: "qword", data: "18446744073709551615" })), "0xffffffffffffffff (18446744073709551615)");
  assert.equal(formatData(value({ type: "binary", data: "0aff10" })), "0a ff 10");
  assert.equal(formatData(value({ type: "binary", data: "" })), "(zero-length binary value)");
  assert.equal(formatData(value({ type: "multi_string", data: ["a", "b"] })), "a b");
  assert.equal(formatData(value({ type: "string", data: "cut" }, { truncated: true })), "cut…");
  assert.equal(formatData({ name: "", type: "string", data: "", size: 0 }), "(value not set)");
  assert.equal(typeName({ type: "other", data: { kind: 8, hex: "" } }), "REG_RESOURCE_LIST");
  assert.equal(typeName({ type: "expand_string", data: "" }), "REG_EXPAND_SZ");
  assert.equal(editable(value({ type: "string", data: "x" }, { truncated: true })), false);
  assert.equal(editable(value({ type: "other", data: { kind: 0, hex: "" } })), false);
  const sorted = sortedValues([{ name: "b" }, { name: "" }, { name: "A" }]).map((each) => each.name);
  assert.deepEqual(sorted, ["", "A", "b"]);
});

test("the editor writes each type, refusing what Windows wouldn't hold", () => {
  assert.deepEqual(dataFromDraft("dword", "ff", "hex"), { data: { type: "dword", data: 255 } });
  assert.deepEqual(dataFromDraft("dword", "0x10", "hex"), { data: { type: "dword", data: 16 } });
  assert.deepEqual(dataFromDraft("dword", "4294967295", "decimal"), { data: { type: "dword", data: 4294967295 } });
  assert.match(dataFromDraft("dword", "100000000", "hex").problem, /at most ffffffff/);
  assert.match(dataFromDraft("dword", "12a", "decimal").problem, /whole number/);
  // A QWORD keeps every digit, past what JavaScript's numbers hold.
  assert.deepEqual(dataFromDraft("qword", "ffffffffffffffff", "hex"), { data: { type: "qword", data: "18446744073709551615" } });
  assert.match(dataFromDraft("qword", "10000000000000000", "hex").problem, /REG_QWORD/);
  assert.deepEqual(dataFromDraft("binary", "0A ff\n10", "hex"), { data: { type: "binary", data: "0aff10" } });
  assert.match(dataFromDraft("binary", "abc", "hex").problem, /two/);
  assert.match(dataFromDraft("binary", "zz", "hex").problem, /hexadecimal/);
  assert.deepEqual(dataFromDraft("multi_string", "one\n\ntwo\r\n", "hex"), { data: { type: "multi_string", data: ["one", "two"] } });
  assert.match(dataFromDraft("string", "a\0b", "hex").problem, /NUL/);
  assert.match(dataFromDraft("binary", "00".repeat(MAX_REGISTRY_VALUE_BYTES + 1), "hex").problem, /at most/);
  assert.equal(storedBytes({ type: "string", data: "ab" }), 6);
  assert.equal(storedBytes({ type: "multi_string", data: ["a", "b"] }), 10);
});

test("numbers switch base without losing their value", () => {
  assert.equal(draftOf({ type: "dword", data: 255 }, "hex"), "ff");
  assert.equal(draftOf({ type: "qword", data: "18446744073709551615" }, "decimal"), "18446744073709551615");
  assert.equal(draftOf({ type: "binary", data: "0aff" }), "0a ff");
  assert.equal(draftOf({ type: "multi_string", data: ["a", "b"] }), "a\nb");
  assert.equal(rebase("ff", "hex", "decimal"), "255");
  assert.equal(rebase("255", "decimal", "hex"), "ff");
  assert.equal(rebase("nope", "decimal", "hex"), "nope");
});

test("only Windows has a registry, and others say why", () => {
  assert.equal(unavailableReason("registry", "windows"), null);
  assert.match(unavailableReason("registry", "linux"), /Windows/);
  assert.match(unavailableReason("registry", "macos"), /Windows/);
});
