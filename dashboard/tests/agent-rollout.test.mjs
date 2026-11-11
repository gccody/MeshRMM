import assert from "node:assert/strict";
import test from "node:test";
import { REACH_TEXT, ROLLOUT_STEPS, loweringText, nextSteps, parsePercent, progressLines, stageText } from "../features/settings/agent-rollout.ts";
import { SETTINGS_TABS } from "../features/settings/general-settings.ts";

const rollout = (fields = {}) => ({ release: "1.2.0", mode: "staged", percent: 10, paused: false, ...fields });

test("Agent updates are a settings category", () => {
  assert.ok(SETTINGS_TABS.some((tab) => tab.id === "agent-updates" && tab.label === "Agent updates"));
});

test("the rollout advances to the steps above its stage", () => {
  assert.deepEqual(ROLLOUT_STEPS, [10, 25, 50, 100]);
  assert.deepEqual(nextSteps(1), [10, 25, 50, 100]);
  assert.deepEqual(nextSteps(10), [25, 50, 100]);
  assert.deepEqual(nextSteps(30), [50, 100]);
  assert.deepEqual(nextSteps(100), []);
});

test("a custom stage is a whole percentage from 1 to 100", () => {
  assert.equal(parsePercent("1"), 1);
  assert.equal(parsePercent(" 35 "), 35);
  assert.equal(parsePercent("35%"), 35);
  assert.equal(parsePercent("100"), 100);
  for (const invalid of ["", "0", "101", "-5", "2.5", "1e2", "ten", "1000"]) assert.equal(parsePercent(invalid), undefined, invalid);
});

test("the stage reads as who is offered the release", () => {
  assert.equal(stageText(rollout({ mode: "everyone" })), "Every device is offered Agent 1.2.0 as soon as this server ships it.");
  assert.equal(stageText(rollout()), "10% of devices are offered Agent 1.2.0.");
  assert.equal(stageText(rollout({ percent: 100 })), "Every device is offered Agent 1.2.0.");
  assert.match(stageText(rollout({ percent: 25, paused: true })), /^Paused at 25%\. No device that doesn’t have Agent 1\.2\.0 yet is offered it; one already installing it finishes\.$/);
  assert.match(stageText(rollout({ release: null })), /a new Agent release/);
});

test("the counts read as where devices stand with the release", () => {
  assert.deepEqual(progressLines("1.2.0", { on_release: 1, offered: 1_250, waiting: 0 }), [
    "1 device on 1.2.0 or newer",
    "1,250 devices offered it and not on it yet",
    "0 devices not offered it yet",
  ]);
});

test("lowering the stage says what it does and doesn't undo", () => {
  assert.equal(loweringText(25, 50), null);
  assert.equal(loweringText(25, 25), null);
  assert.equal(loweringText(50, 10), "Devices from 10% to 50% that don’t have the release yet are no longer offered it. Those that installed it keep it.");
});

test("the page says when a change reaches devices", () => {
  assert.match(REACH_TEXT, /next update check, within about 6½ hours/);
  assert.match(REACH_TEXT, /same devices go first/);
});
