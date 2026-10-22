import assert from "node:assert/strict";
import test from "node:test";
import { applyInventoryDelta, parseAgentEvent } from "../features/agents/model.ts";
import { describeTagTarget, hasTags, tagChanges, tagCoverage, tagNameProblem, tagsOf } from "../features/tags/model.ts";

const tags = [
  { id: "l", name: "London", color: "green" },
  { id: "s", name: "Servers", color: "blue" },
];

test("tag events keep the inventory's tags sorted by name", () => {
  let inventory = { agents: [], companies: [], tags: [] };
  inventory = applyInventoryDelta(inventory, { type: "tag_upsert", revision: 1, tag: tags[1] });
  inventory = applyInventoryDelta(inventory, { type: "tag_upsert", revision: 2, tag: tags[0] });
  assert.deepEqual(inventory.tags.map((tag) => tag.name), ["London", "Servers"]);
  inventory = applyInventoryDelta(inventory, { type: "tag_upsert", revision: 3, tag: { ...tags[1], name: "app servers" } });
  assert.deepEqual(inventory.tags.map((tag) => tag.name), ["app servers", "London"]);
  inventory = applyInventoryDelta(inventory, { type: "tag_deleted", revision: 4, tag_id: "l" });
  assert.deepEqual(inventory.tags.map((tag) => tag.id), ["s"]);
  assert.deepEqual(parseAgentEvent({ type: "tag_upsert", revision: 5, tag: tags[0] }), { type: "tag_upsert", revision: 5, tag: tags[0] });
});

test("devices match any or all of some tags", () => {
  const device = { tag_ids: ["s"] };
  assert.equal(hasTags(device, ["s", "l"], "any"), true);
  assert.equal(hasTags(device, ["s", "l"], "all"), false);
  assert.equal(hasTags({ tag_ids: ["l", "s"] }, ["s", "l"], "all"), true);
  assert.equal(hasTags({}, ["s"], "any"), false);
  assert.equal(hasTags({}, [], "all"), true, "no tags chosen matches every device");
  assert.deepEqual(tagsOf(["s", "gone", "l"], tags).map((tag) => tag.name), ["London", "Servers"]);
});

test("tag names are unique ignoring case and fit on one line", () => {
  assert.equal(tagNameProblem("  ", tags), "Name the tag.");
  assert.equal(tagNameProblem("servers", tags), "A tag with this name already exists.");
  assert.equal(tagNameProblem("SERVERS", tags, "s"), null, "a tag may keep its own name");
  assert.match(tagNameProblem("a\nb", tags), /one line/u);
  assert.match(tagNameProblem("x".repeat(61), tags), /60/u);
  assert.equal(tagNameProblem("Paris", tags), null);
});

test("targets on tags read as words", () => {
  assert.equal(describeTagTarget(["s", "l"], "any", tags), "Tagged London or Servers");
  assert.equal(describeTagTarget(["s", "l"], "all", tags), "Tagged London and Servers");
  assert.equal(describeTagTarget(["gone"], "any", tags), "Deleted tags");
  assert.equal(describeTagTarget([], "any", tags), "No tags");
});

test("bulk tagging adds and removes only what the devices need", () => {
  const devices = [{ tag_ids: ["s"] }, { tag_ids: ["s", "l"] }, {}];
  assert.equal(tagCoverage(devices, "s"), "some");
  assert.equal(tagCoverage(devices.slice(0, 2), "s"), "all");
  assert.equal(tagCoverage(devices, "x"), "none");
  assert.deepEqual(tagChanges(devices, new Map([["s", true], ["l", false], ["x", false]])), { add: ["s"], remove: ["l"] });
  assert.deepEqual(tagChanges(devices.slice(0, 2), new Map([["s", true]])), { add: [], remove: [] });
});
