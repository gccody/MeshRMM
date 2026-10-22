import assert from "node:assert/strict";
import test from "node:test";
import {
  applyJobUpdate,
  countsSummary,
  coveredAgents,
  describeSchedule,
  describeTarget,
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
});

test("policy drafts check the server's rules and become its input", () => {
  const draft = { ...policyDraft(null, "Europe/Paris"), name: " Nightly ", scriptId: "s" };
  assert.equal(policyDraftProblem(draft), null);
  assert.deepEqual(policyInput(draft), {
    name: "Nightly", enabled: true, action: "script", script_id: "s", run_as: "system", target: "all", device_ids: [], company_ids: [],
    trigger: "daily", offline: "wait", catch_up_hours: 24, channel_ids: [], time: "03:00", time_zone: "Europe/Paris",
  });
  const problems = [
    { name: "" },
    { scriptId: "" },
    { target: "companies" },
    { target: "devices" },
    { trigger: "enroll", target: "devices", deviceIds: ["d"] },
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
    trigger: "interval", offline: "skip", catch_up_hours: 12, channel_ids: ["c"], interval_minutes: 90,
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
