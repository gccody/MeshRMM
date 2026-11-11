import assert from "node:assert/strict";
import test from "node:test";
import { DEFAULT_DEVICE_FILTERS, filterAgents, parseDeviceFilters, serializeDeviceFilters } from "../features/agents/device-filters.ts";
import {
  authorText,
  displayValue,
  draftOf,
  draftValue,
  fieldFilterLabel,
  fieldVariable,
  filterOps,
  keyProblem,
  parseFieldFilter,
  parseOptions,
  suggestedKey,
} from "../features/custom-fields/model.ts";

const field = (type, options = []) => ({
  id: "f", key: "f", label: "Field", description: "", type, options,
  applies_to: "devices", position: 0, script_access: "none", created_at: 0, updated_at: 0,
});

test("a label suggests a key scripts can name", () => {
  assert.equal(suggestedKey("Contract tier"), "contract_tier");
  assert.equal(suggestedKey("  Last DR test (date) "), "last_dr_test_date");
  assert.equal(suggestedKey("2nd office"), "nd_office", "a key starts with a letter");
  assert.equal(suggestedKey("Café № 1"), "cafe_no_1");
  assert.equal(keyProblem("contract_tier"), null);
  assert.match(keyProblem("Contract"), /lowercase letter/);
  assert.match(keyProblem(""), /key/);
  assert.match(keyProblem("a".repeat(65)), /at most 64/);
  assert.equal(fieldVariable("contract_tier"), "MESHRMM_FIELD_CONTRACT_TIER");
});

test("options are typed one per line, each once", () => {
  assert.deepEqual(parseOptions(" Gold \n\nSilver\n"), { options: ["Gold", "Silver"], problem: null });
  assert.match(parseOptions("Gold\ngold").problem, /twice/);
});

test("drafts become values of the field's type, or say what's wrong", () => {
  assert.deepEqual(draftValue(field("number"), " 12.5 "), { value: 12.5 });
  assert.match(draftValue(field("number"), "twelve").problem, /number/);
  assert.deepEqual(draftValue(field("number"), ""), { value: null }, "empty clears");
  assert.deepEqual(draftValue(field("checkbox"), true), { value: true });
  assert.deepEqual(draftValue(field("checkbox"), ""), { value: null });
  assert.deepEqual(draftValue(field("date"), "2024-02-29"), { value: "2024-02-29" });
  assert.match(draftValue(field("date"), "2023-02-29").problem, /YYYY-MM-DD/);
  assert.deepEqual(draftValue(field("dropdown", ["Gold", "Silver"]), "silver"), { value: "Silver" });
  assert.match(draftValue(field("dropdown", ["Gold"]), "Bronze").problem, /options/);
  assert.deepEqual(draftValue(field("multi_select", ["A", "B", "C"]), ["C", "A"]), { value: ["A", "C"] }, "in the field's order");
  assert.deepEqual(draftValue(field("multi_select", ["A"]), []), { value: null });
  assert.deepEqual(draftValue(field("url"), "https://example.com"), { value: "https://example.com" });
  assert.match(draftValue(field("url"), "javascript:alert(1)").problem, /web address/);
  assert.match(draftValue(field("text"), "two\nlines").problem, /one line/);
  assert.match(draftValue(field("text"), "x".repeat(1001)).problem, /1,000/);
  assert.deepEqual(draftValue(field("multiline"), "a\r\nb\n"), { value: "a\nb" });
  assert.deepEqual(draftValue(field("secret"), " p@ss "), { value: " p@ss " }, "secrets keep their spaces");
  assert.deepEqual(draftOf(field("multi_select", ["A"]), null), []);
  assert.equal(draftOf(field("checkbox"), false), false);
  assert.equal(draftOf(field("number"), 3), "3");
});

test("values read as text, and secrets never show", () => {
  assert.equal(displayValue(field("checkbox"), true, true), "Yes");
  assert.equal(displayValue(field("multi_select", ["A", "B"]), ["A", "B"], true), "A, B");
  assert.equal(displayValue(field("number"), 1234.5, true), "1,234.5");
  assert.equal(displayValue(field("secret"), null, true), "••••••••");
  assert.equal(displayValue(field("secret"), null, false), "");
  assert.equal(authorText({ kind: "script", run_id: "r", script_name: "Backup check" }), "the script “Backup check”");
  assert.equal(authorText({ kind: "user", user_id: "u", name: "Ada" }), "Ada");
});

test("the wall filters by a field, kept in its address", () => {
  assert.deepEqual(filterOps("secret"), ["not_empty", "empty"]);
  assert.deepEqual(filterOps("multi_select"), ["contains", "not_empty", "empty"]);
  assert.ok(!filterOps("number").includes("contains"));
  const filter = { key: "backup_vendor", op: "contains", value: "veeam & co" };
  const search = serializeDeviceFilters({ ...DEFAULT_DEVICE_FILTERS, company: "acme", field: filter });
  assert.deepEqual(parseDeviceFilters(new URLSearchParams(search)).field, filter);
  assert.deepEqual(parseDeviceFilters(new URLSearchParams("field=vendor&field_op=empty&field_value=x")).field, { key: "vendor", op: "empty", value: "" });
  for (const wrong of ["field=Vendor&field_op=empty", "field=vendor&field_op=near", "field=vendor&field_op=equals"]) {
    assert.equal(parseFieldFilter(new URLSearchParams(wrong)), null, wrong);
  }
  assert.equal(parseDeviceFilters(new URLSearchParams("")).field, undefined);
  assert.equal(fieldFilterLabel(filter, [{ ...field("text"), key: "backup_vendor", label: "Backup vendor" }]), "Backup vendor contains “veeam & co”");
  assert.equal(fieldFilterLabel(null, []), "Any field");

  const agents = ["a", "b", "c"].map((id) => ({ id, name: id, connected: true, company_id: null, system: null, tag_ids: [] }));
  const ids = (matches) => filterAgents(agents, { ...DEFAULT_DEVICE_FILTERS, field: filter }, [], matches).map((agent) => agent.id);
  assert.deepEqual(ids(new Set(["a", "c"])), ["a", "c"], "the devices the server matched");
  assert.deepEqual(ids(null), [], "none until the server answers");
  assert.deepEqual(filterAgents(agents, DEFAULT_DEVICE_FILTERS).map((agent) => agent.id), ["a", "b", "c"]);
});
