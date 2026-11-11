import assert from "node:assert/strict";
import test from "node:test";
import {
  describeFrequency,
  describeRequester,
  describeScope,
  formatSize,
  isWorking,
  parseEmails,
  runDraft,
  runDraftProblem,
  runInput,
  scheduleDraft,
  scheduleDraftProblem,
  scheduleInput,
  tableDownload,
} from "../features/reports/model.ts";
import { canView, navViewFor, viewForPath } from "../features/workspace/views.ts";

const account = (permissions) => ({ permissions, is_administrator: false });

const templates = {
  templates: [
    { id: "executive_summary", title: "Executive summary", description: "", uses_period: true, uses_stale_days: true },
    { id: "hardware_inventory", title: "Hardware asset inventory", description: "", uses_period: false, uses_stale_days: false },
  ],
  max_stale_days: 365,
  max_custom_days: 366,
};
const zones = ["Europe/London", "UTC"];
const today = new Date(2026, 2, 15);

test("the Reports page opens to who may see reports", () => {
  assert.equal(canView(account(["reports.view"]), "reports"), true);
  assert.equal(canView(account(["devices.view"]), "reports"), false);
  assert.equal(viewForPath("/reports/"), "reports");
  assert.equal(navViewFor("reports"), "reports");
});

test("a run asks for the period only for reports that cover one", () => {
  const draft = runDraft("executive_summary", "UTC", 30, today);
  assert.equal(draft.from, "2026-03-08");
  assert.equal(draft.to, "2026-03-15");
  assert.equal(runDraftProblem(draft, templates, zones, today), null);
  assert.deepEqual(runInput(draft), {
    template: "executive_summary", target: "all", company_ids: [], tag_ids: [], tag_match: "any",
    period: "last_30_days", time_zone: "UTC", stale_days: 30,
  });
  const custom = { ...draft, period: "custom" };
  assert.deepEqual([runInput(custom).from, runInput(custom).to], ["2026-03-08", "2026-03-15"]);
  assert.equal(runDraftProblem({ ...custom, to: "2026-03-01" }, templates, zones, today), "The last day comes before the first.");
  assert.equal(runDraftProblem({ ...custom, to: "2026-03-16" }, templates, zones, today), "The range can't end after today.");
  assert.equal(runDraftProblem({ ...custom, from: "2025-01-01" }, templates, zones, today), "A range covers at most 366 days.");
  assert.equal(runDraftProblem({ ...draft, staleDays: "0" }, templates, zones, today), "Devices count as not seen after 1 to 365 days.");
  assert.equal(runDraftProblem({ ...draft, target: "companies" }, templates, zones, today), "Choose at least one company.");
  assert.equal(runDraftProblem({ ...draft, timeZone: "Mars/Olympus" }, templates, zones, today), "Choose a time zone.");
  // The inventory is as the devices are now: no dates, no stale days.
  const inventory = { ...custom, template: "hardware_inventory", to: "2020-01-01", staleDays: "" };
  assert.equal(runDraftProblem(inventory, templates, zones, today), null);
});

test("schedules check their recipients and timing", () => {
  const draft = { ...scheduleDraft(null, "Europe/London", 30), name: "Weekly" };
  assert.equal(scheduleDraftProblem(draft, zones, 365), null);
  assert.deepEqual(parseEmails(" A@Example.com, b@example.com\nb@example.com "), ["a@example.com", "b@example.com"]);
  assert.equal(scheduleDraftProblem({ ...draft, emails: "nobody" }, zones, 365), "“nobody” isn't an email address.");
  assert.equal(scheduleDraftProblem({ ...draft, frequency: "monthly", dayOfMonth: "31" }, zones, 365), "Choose a day of the month from 1 to 28.");
  assert.equal(scheduleDraftProblem({ ...draft, name: " " }, zones, 365), "Name the schedule.");
  const input = scheduleInput({ ...draft, frequency: "monthly", dayOfMonth: "1", emails: "x@example.com", userIds: ["u1"] });
  assert.equal(input.weekday, null);
  assert.equal(input.day_of_month, 1);
  assert.deepEqual(input.emails, ["x@example.com"]);
  assert.deepEqual(input.user_ids, ["u1"]);
  assert.equal(scheduleInput(draft).weekday, 1);

  const saved = {
    ...input, id: "s", name: "Weekly", frequency: "weekly", weekday: 3, day_of_month: null, time: "07:30",
    time_zone: "Europe/London", stale_days: 30, company_ids: [], tag_ids: [], owner: "a", owned_by_you: true,
  };
  assert.equal(describeFrequency(saved), "Every Wednesday at 07:30 (Europe/London)");
  assert.equal(describeFrequency({ ...saved, frequency: "monthly", day_of_month: 5 }), "On day 5 of each month at 07:30 (Europe/London)");
  assert.equal(scheduleDraft(saved, "UTC", 30).weekday, 3);
});

test("reports say who made them, and when to look again", () => {
  assert.equal(describeRequester({ trigger: "manual", requested_by: "ada@example.com", requested_by_you: true, schedule_name: null }), "You");
  assert.equal(describeRequester({ trigger: "schedule", requested_by: "Schedule “Weekly”", requested_by_you: false, schedule_name: "Weekly" }), "Schedule “Weekly”");
  assert.equal(describeRequester({ trigger: "run_now", requested_by: "bo@example.com", requested_by_you: false, schedule_name: "Weekly" }), "Run now by bo@example.com, from “Weekly”");
  assert.equal(isWorking([{ status: "succeeded", delivery: null }]), false);
  assert.equal(isWorking([{ status: "succeeded", delivery: "pending" }]), true);
  assert.equal(isWorking([{ status: "queued", delivery: null }]), true);
  assert.equal(formatSize(1_500_000), "1.5 MB");
  assert.equal(tableDownload("r 1", "devices"), "/v1/reports/r%201/tables/devices.csv");
  const companies = [{ id: "c1", name: "Acme" }];
  assert.equal(describeScope("companies", ["c1", "gone"], [], "any", companies, []), "Acme, a deleted company");
  assert.equal(describeScope("all", [], [], "any", companies, []), "Every device");
});
