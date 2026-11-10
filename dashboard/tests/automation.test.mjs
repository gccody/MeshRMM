import assert from "node:assert/strict";
import test from "node:test";
import {
  applyJobDevicesAnswer,
  applyJobUpdate,
  countsSummary,
  devicesRefreshDelay,
  jobChanged,
  jobDevicesKey,
  jobDevicesPath,
  moreJobDevices,
  coveredAgents,
  describeSchedule,
  describeTarget,
  applicationProblems,
  deviceResult,
  isJobFinished,
  jobPreview,
  jobStarter,
  outcomeTone,
  parseJobEvent,
  policyDraft,
  policyDraftProblem,
  policyInput,
  previewText,
  progressLabel,
  progressSegments,
  timeZones,
  currentZoneName,
  waitHoursProblem,
} from "../features/automation/model.ts";
import { canView, homeView, navViewFor, viewForPath } from "../features/workspace/views.ts";

const counts = (change = {}) => ({
  total: 0, queued: 0, waiting: 0, running: 0, succeeded: 0, failed: 0, timed_out: 0, no_result: 0, skipped: 0, missed: 0, cancelled: 0,
  ...change,
});

const job = (id, change = {}) => ({
  id,
  kind: "script",
  item_id: "s",
  item_name: "Clean up",
  target: "all",
  target_label: "All devices",
  offline: "wait",
  expires_at_unix_ms: 0,
  requested_by: "Ada",
  policy_id: null,
  policy_name: null,
  created_at_unix_ms: 1,
  finished_at_unix_ms: null,
  counts: counts({ total: 1, running: 1 }),
  requested_by_you: true,
  can_cancel: true,
  ...change,
});

const agent = (id, platform, connected = true, company_id = null) => ({ id, name: id, platform, connected, company_id });

test("counts read as results first, and the bar as fractions in order", () => {
  const value = counts({ total: 10, succeeded: 5, failed: 1, timed_out: 1, skipped: 2, waiting: 1 });
  assert.equal(countsSummary(value), "5 succeeded · 1 failed · 1 timed out · 2 skipped · 1 waiting");
  assert.equal(countsSummary(counts()), "No devices");
  assert.equal(progressLabel(value), "9 of 10 done");
  const segments = progressSegments(value);
  assert.deepEqual(segments.map((segment) => segment.tone), ["success", "problem", "muted"]);
  assert.equal(segments[1].start, 0.5);
  assert.equal(segments[1].fraction, 0.2);
  assert.equal(segments[2].start, 0.7);
  assert.equal(isJobFinished(job("j", { counts: value })), false);
  assert.equal(isJobFinished(job("j", { counts: counts({ total: 2, succeeded: 2 }) })), true);
  assert.equal(isJobFinished(job("j", { finished_at_unix_ms: 5 })), true);
});

test("outcomes have tones and devices say why", () => {
  assert.equal(outcomeTone("succeeded"), "success");
  assert.equal(outcomeTone("no_result"), "problem");
  assert.equal(outcomeTone("waiting"), "pending");
  assert.equal(outcomeTone("skipped"), "muted");
  assert.equal(deviceResult({ outcome: "failed", exit_code: 3, detail: null, path: null }), "Exit code 3");
  assert.equal(deviceResult({ outcome: "skipped", exit_code: null, detail: "macOS has no PowerShell interpreter", path: null }), "macOS has no PowerShell interpreter");
  assert.equal(deviceResult({ outcome: "succeeded", exit_code: null, detail: null, path: "C:\\Users\\Public\\x.msi" }), "C:\\Users\\Public\\x.msi");
});

test("jobs name who started them", () => {
  assert.equal(jobStarter(job("j")), "You");
  assert.equal(jobStarter(job("j", { requested_by_you: false })), "Ada");
  assert.equal(jobStarter(job("j", { policy_name: "Nightly" })), "Policy “Nightly”");
});

test("socket job events update the list in place, newest first", () => {
  assert.deepEqual(parseJobEvent({ type: "jobs_resync" }), { type: "jobs_resync" });
  assert.equal(parseJobEvent({ type: "job_update", job: { id: 1 } }), null);
  assert.equal(parseJobEvent({ type: "snapshot" }), null);
  const updated = job("a", { created_at_unix_ms: 1, counts: counts({ total: 1, succeeded: 1 }) });
  assert.deepEqual(parseJobEvent({ type: "job_update", job: updated }), { type: "job_update", job: updated });
  const list = [job("b", { created_at_unix_ms: 2 }), job("a", { created_at_unix_ms: 1 })];
  const next = applyJobUpdate(list, updated);
  assert.deepEqual(next.map((item) => item.id), ["b", "a"]);
  assert.equal(next[1].counts.succeeded, 1);
  assert.deepEqual(applyJobUpdate(list, job("c", { created_at_unix_ms: 3 })).map((item) => item.id), ["c", "b", "a"]);
  assert.equal(applyJobUpdate(list, job("c", { policy_id: "x" }), "p").length, 2, "another policy's job stays out");
});

test("the preview counts devices, skips and offline ones", () => {
  const agents = [agent("win", "windows"), agent("mac", "macos", false, "acme"), agent("linux", "linux", true, "acme"), agent("old", "windows", false)];
  assert.deepEqual(coveredAgents(agents, { kind: "companies", deviceIds: [], companyIds: ["acme"] }).map((item) => item.id), ["mac", "linux"]);
  assert.deepEqual(coveredAgents(agents, { kind: "devices", deviceIds: ["win", "gone"], companyIds: [] }).map((item) => item.id), ["win"]);
  const tagged = [{ ...agents[0], tag_ids: ["s", "l"] }, { ...agents[1], tag_ids: ["l"] }, agents[2]];
  const byTags = (tagIds, tagMatch) => coveredAgents(tagged, { kind: "tags", deviceIds: [], companyIds: [], tagIds, tagMatch }).map((item) => item.id);
  assert.deepEqual(byTags(["s", "l"], "any"), ["win", "mac"]);
  assert.deepEqual(byTags(["s", "l"], "all"), ["win"]);
  assert.deepEqual(byTags([], "any"), [], "no tags cover no devices");
  const all = { kind: "all", deviceIds: [], companyIds: [] };
  const preview = jobPreview(agents, all, "shell");
  assert.deepEqual(preview, { devices: 4, skipped: 2, offline: 1 });
  assert.equal(
    previewText(preview, "shell", "wait", 24),
    "Runs on 4 devices. 2 will be skipped: they have no Shell (zsh/bash) interpreter. 1 is offline and will run if it connects within 24 hours.",
  );
  assert.equal(previewText(jobPreview(agents, all, null), null, "skip", 24), "Runs on 4 devices. 2 are offline and will be skipped.");
  assert.equal(previewText({ devices: 0, skipped: 0, offline: 0 }, null, "skip", 24), "No devices are chosen.");
  assert.equal(waitHoursProblem("24"), null);
  assert.ok(waitHoursProblem("0"));
  assert.ok(waitHoursProblem("169"));
  assert.ok(waitHoursProblem("1.5"));
});

test("schedules and targets read as words", () => {
  assert.equal(describeSchedule({ trigger: "interval", interval_minutes: 15 }), "Every 15 minutes");
  assert.equal(describeSchedule({ trigger: "interval", interval_minutes: 60 }), "Every hour");
  assert.equal(describeSchedule({ trigger: "interval", interval_minutes: 360 }), "Every 6 hours");
  assert.equal(describeSchedule({ trigger: "daily", time: "03:00", time_zone: "Europe/Paris" }), "Daily at 03:00 (Europe/Paris)");
  assert.equal(describeSchedule({ trigger: "weekly", time: "08:00", weekdays: [1, 5], time_zone: "UTC" }), "Weekly on Mon, Fri at 08:00 (UTC)");
  assert.equal(describeSchedule({ trigger: "once", run_at: "2026-11-01T06:15", time_zone: "Asia/Tokyo" }), "Once on 2026-11-01 at 06:15 (Asia/Tokyo)");
  assert.equal(describeSchedule({ trigger: "enroll" }), "When a device enrolls");
  const companies = new Map([["a", "Acme"]]);
  assert.equal(describeTarget({ target: "all", company_ids: [], device_ids: [] }, companies, new Map()), "All devices");
  assert.equal(describeTarget({ target: "companies", company_ids: ["a", "b"], device_ids: [] }, companies, new Map()), "Acme, Deleted company");
  assert.equal(describeTarget({ target: "devices", company_ids: [], device_ids: ["1", "2", "3", "4"] }, companies, new Map()), "4 devices");
  const tags = [{ id: "l", name: "London", color: "green" }, { id: "s", name: "Servers", color: "blue" }];
  assert.equal(describeTarget({ target: "tags", company_ids: [], device_ids: [], tag_ids: ["s", "l"], tag_match: "any" }, companies, new Map(), tags), "Tagged London or Servers");
});

test("policy drafts check the server's rules and become its input", () => {
  const draft = { ...policyDraft(null, "Europe/Paris"), name: " Nightly ", scriptId: "s" };
  assert.equal(policyDraftProblem(draft), null);
  assert.deepEqual(policyInput(draft), {
    name: "Nightly", enabled: true, action: "script", script_id: "s", run_as: "system", target: "all", device_ids: [], company_ids: [],
    tag_ids: [], tag_match: "any", trigger: "daily", offline: "wait", catch_up_hours: 24, channel_ids: [], time: "03:00", time_zone: "Europe/Paris",
  });
  const problems = [
    { name: "" },
    { scriptId: "" },
    { target: "companies" },
    { target: "devices" },
    { target: "tags" },
    { trigger: "enroll", target: "devices", deviceIds: ["d"] },
    { trigger: "enroll", target: "tags", tagIds: ["t"] },
    { trigger: "interval", intervalValue: "4", intervalUnit: "minutes" },
    { trigger: "interval", intervalValue: "700", intervalUnit: "hours" },
    { time: "3:00" },
    { trigger: "weekly", weekdays: [] },
    { trigger: "once", runAt: "2026-02-30T10:00" },
    { timeZone: "" },
    { catchUpHours: "0" },
  ];
  for (const change of problems) assert.ok(policyDraftProblem({ ...draft, ...change }), JSON.stringify(change));
  assert.ok(policyDraftProblem({ ...draft, timeZone: "Mars/Base" }, ["Europe/Paris"]));
  assert.equal(policyDraftProblem({ ...draft, catchUpHours: "0", offline: "skip" }), null, "skipping needs no catch-up time");
  const interval = policyInput({ ...draft, trigger: "interval", intervalValue: "2", intervalUnit: "hours", offline: "skip", catchUpHours: "500" });
  assert.equal(interval.interval_minutes, 120);
  assert.equal(interval.catch_up_hours, 24);
  assert.equal(interval.time_zone, undefined);
  const weekly = policyInput({ ...draft, trigger: "weekly", weekdays: [5, 1], target: "companies", companyIds: ["a"] });
  assert.deepEqual(weekly.weekdays, [1, 5]);
  assert.deepEqual(weekly.company_ids, ["a"]);
  const tagged = policyInput({ ...draft, target: "tags", tagIds: ["t"], tagMatch: "all", companyIds: ["a"] });
  assert.deepEqual([tagged.target, tagged.tag_ids, tagged.tag_match, tagged.company_ids], ["tags", ["t"], "all", []]);
  const once = policyInput({ ...draft, trigger: "once", runAt: "2026-11-01T06:15" });
  assert.equal(once.run_at, "2026-11-01T06:15");
  assert.equal(once.time, undefined);
});

test("a saved policy's draft round-trips", () => {
  const policy = {
    id: "p", name: "Weekly", enabled: false, script_id: "s", run_as: "user", target: "devices", device_ids: ["d"], company_ids: [],
    trigger: "interval", interval_minutes: 90, offline: "skip", catch_up_hours: 12, channel_ids: ["c"],
  };
  const draft = policyDraft(policy, "UTC");
  assert.equal(draft.intervalValue, "90");
  assert.equal(draft.intervalUnit, "minutes");
  assert.deepEqual(policyInput(draft), {
    name: "Weekly", enabled: false, action: "script", script_id: "s", run_as: "user", target: "devices", device_ids: ["d"], company_ids: [],
    tag_ids: [], tag_match: "any", trigger: "interval", offline: "skip", catch_up_hours: 12, channel_ids: ["c"], interval_minutes: 90,
  });
  assert.equal(policyDraft({ ...policy, interval_minutes: 120 }, "UTC").intervalUnit, "hours");
  assert.deepEqual(timeZones(["Europe/Paris"]), ["Europe/Paris", "UTC"]);
  assert.deepEqual(timeZones(["UTC"]), ["UTC"]);
  // Browsers list some zones by ICU's older names; the current ones are offered.
  assert.deepEqual(timeZones(["Asia/Calcutta", "Asia/Kolkata", "Europe/Kiev"]), ["Asia/Kolkata", "Europe/Kyiv", "UTC"]);
  assert.equal(currentZoneName("America/Godthab"), "America/Nuuk");
  assert.equal(currentZoneName("Europe/London"), "Europe/London");
});

test("jobs and policies have pages under Jobs", () => {
  const technician = { permissions: ["devices.view", "scripts.run"], is_administrator: false };
  assert.equal(viewForPath("/jobs"), "jobs");
  assert.equal(viewForPath("/job"), "job");
  assert.equal(viewForPath("/policies"), "policies");
  assert.equal(canView(technician, "jobs"), true);
  assert.equal(canView(technician, "policies"), false);
  assert.equal(canView({ permissions: ["automation.manage"], is_administrator: false }, "policies"), true);
  assert.equal(canView({ permissions: ["audit.view"], is_administrator: false }, "job"), true);
  assert.equal(navViewFor("job"), "jobs");
  assert.equal(navViewFor("policies"), "jobs");
  assert.equal(homeView({ permissions: ["audit.view"], is_administrator: false }), "audit");
  assert.equal(homeView({ permissions: ["automation.manage"], is_administrator: false }), "jobs");
});

test("a job device names the applications that failed or were skipped", () => {
  assert.deepEqual(applicationProblems({}), []);
  assert.deepEqual(
    applicationProblems({ applications: [{ name: "Firefox", outcome: "failed", error: "winget exited with 1" }, { name: "Zoom", outcome: "skipped", error: null }] }),
    ["Firefox failed: winget exited with 1", "Zoom skipped"],
  );
});

const part = (id, outcome = "running") => ({
  device_id: id, device_name: id, platform: "linux", outcome, detail: null, run_id: null, delivery_id: null,
  exit_code: null, path: null, started_at_unix_ms: null, completed_at_unix_ms: null,
});
const start = (key, ticket, ids, matching, end = false, outcome = "running") =>
  ({ kind: "start", key, ticket, devices: ids.map((id) => part(id, outcome)), matching, end });
const more = (key, ticket, after, ids, matching, end = false) =>
  ({ kind: "more", key, ticket, after, devices: ids.map((id) => part(id)), matching, end });
const shown = (list) => list.devices.map((device) => device.device_id);

test("a job's devices are asked for a page at a time, by outcome", () => {
  assert.equal(jobDevicesPath("j 1", null, null), "/v1/jobs/j%201/devices?limit=100");
  assert.equal(jobDevicesPath("j", "failed", "d9", 200), "/v1/jobs/j/devices?outcome=failed&after=d9&limit=200");
  assert.equal(jobDevicesKey(null), "all");
  assert.equal(jobDevicesKey("no_result"), "no_result");
});

test("show more appends the next page after the last row, without repeats", () => {
  const first = applyJobDevicesAnswer(null, start("all", 1, ["a", "b"], 5), "all");
  assert.deepEqual(shown(first), ["a", "b"]);
  assert.equal(moreJobDevices(first), 3);
  // "b" moved between the pages and comes again: it stays where it was.
  const second = applyJobDevicesAnswer(first, more("all", 2, "b", ["b", "c", "d"], 5), "all");
  assert.deepEqual(shown(second), ["a", "b", "c", "d"]);
  const last = applyJobDevicesAnswer(second, more("all", 3, "d", ["e"], 5, true), "all");
  assert.deepEqual(shown(last), ["a", "b", "c", "d", "e"]);
  assert.equal(moreJobDevices(last), 0, "the end was reached");
  // A page asked after a row that is no longer the last can't be placed.
  assert.equal(applyJobDevicesAnswer(second, more("all", 4, "b", ["x"], 5), "all"), second);
  // Nor one of another chip.
  assert.equal(applyJobDevicesAnswer(second, more("failed", 4, "d", ["x"], 5), "all"), second);
});

test("out-of-order answers are dropped", () => {
  // The chip changed while the first answer was on its way.
  assert.equal(applyJobDevicesAnswer(null, start("all", 1, ["a"], 1), "failed"), null);
  const failed = applyJobDevicesAnswer(null, start("failed", 2, ["c"], 1, true, "failed"), "failed");
  assert.equal(applyJobDevicesAnswer(failed, start("all", 1, ["a"], 1), "failed"), failed);
  // Two loads of the same chip: the older, arriving last, is dropped.
  const newer = applyJobDevicesAnswer(failed, start("failed", 4, ["d"], 1, true, "failed"), "failed");
  assert.equal(applyJobDevicesAnswer(newer, start("failed", 3, ["c"], 1, true, "failed"), "failed"), newer);
  // A new chip starts a new list.
  const all = applyJobDevicesAnswer(newer, start("all", 5, ["a", "b"], 2, true), "all");
  assert.deepEqual(shown(all), ["a", "b"]);
});

test("a refresh of the loaded rows keeps their places", () => {
  let list = applyJobDevicesAnswer(null, start("running", 1, ["a", "b", "c"], 6), "running");
  list = applyJobDevicesAnswer(list, more("running", 2, "c", ["d", "e"], 6), "running");
  // "b" finished and left the chip; the refresh reads as many rows as
  // were loaded, which now reach "f".
  const refreshed = applyJobDevicesAnswer(list, start("running", 3, ["a", "c", "d", "e", "f"], 5, true), "running");
  assert.deepEqual(shown(refreshed), ["a", "c", "d", "e", "f"]);
  assert.equal(refreshed.matching, 5);
  // A refresh that covers fewer rows than are loaded, since a page was
  // added while it was on its way, keeps the rows after it.
  const longer = applyJobDevicesAnswer(list, more("running", 4, "e", ["f", "g"], 7), "running");
  const partly = applyJobDevicesAnswer(longer, start("running", 5, ["a", "b", "c"], 7, false, "succeeded"), "running");
  assert.deepEqual(shown(partly), ["a", "b", "c", "d", "e", "f", "g"]);
  assert.equal(partly.devices[1].outcome, "succeeded", "the refreshed rows are the new ones");
  assert.equal(partly.devices[4].outcome, "running", "the rest stay until asked");
  assert.equal(new Set(shown(partly)).size, partly.devices.length, "no row twice");
});

test("the loaded rows are read again at most every few seconds, and only on a change", () => {
  assert.equal(devicesRefreshDelay(10_000, 11_000, 5_000), 4_000);
  assert.equal(devicesRefreshDelay(10_000, 20_000, 5_000), 0);
  const running = job("j");
  assert.equal(jobChanged(running, job("j")), false);
  assert.equal(jobChanged(running, job("j", { counts: counts({ total: 1, succeeded: 1 }) })), true);
  assert.equal(jobChanged(running, job("j", { finished_at_unix_ms: 9 })), true);
});
