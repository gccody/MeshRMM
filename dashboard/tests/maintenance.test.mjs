import assert from "node:assert/strict";
import test from "node:test";
import { DeviceStore } from "../features/agents/device-store.ts";
import { filterAgents, parseDeviceFilters, serializeDeviceFilters, DEFAULT_DEVICE_FILTERS } from "../features/agents/device-filters.ts";
import { describePolicyAction, minutesText, policyDraft, policyDraftProblem, policyInput } from "../features/automation/model.ts";
import { canView, navViewFor, viewForPath } from "../features/workspace/views.ts";
import {
  holdsBack,
  inMaintenance,
  localInput,
  maintenanceLine,
  maintenanceTitle,
  parseLocal,
  replaceWindow,
  startBody,
  startDraft,
  startDraftProblem,
  untilText,
  windowSetBy,
  windowsPath,
} from "../features/maintenance/model.ts";

const NOW = new Date(2026, 5, 7, 10, 0).getTime();
const HOUR = 60 * 60_000;
const FORMAT = { time: (at) => `T${new Date(at).getHours()}`, dateTime: (at) => `D${new Date(at).getDate()}` };

const current = (changes = {}) => ({
  window_id: "w1",
  reason: "Replacing the disk",
  created_by: "Ada",
  starts_at: NOW - HOUR,
  ends_at: NOW + 2 * HOUR,
  automation: "skip",
  windows: 1,
  ...changes,
});

const device = (id, connected, maintenance = null) => ({ id, name: id, platform: "windows", connected, maintenance });

test("a device's maintenance reads as one line", () => {
  assert.equal(maintenanceLine(current(), NOW, FORMAT), "In maintenance until T12: Replacing the disk");
  assert.equal(untilText(NOW + 3 * 24 * HOUR, NOW, FORMAT), `until D${new Date(NOW + 3 * 24 * HOUR).getDate()}`, "another day shows its date");
  assert.equal(untilText(null, NOW, FORMAT), "until someone ends it");
  assert.equal(holdsBack("scans"), "Alerts and scheduled automation are held back; update scans still run.");
  assert.equal(maintenanceTitle(["Desk"]), "Put Desk in maintenance");
  assert.equal(maintenanceTitle(["Desk", "Laptop"]), "Put 2 devices in maintenance");
});

test("only a window in force puts a device in maintenance", () => {
  assert.equal(inMaintenance(device("a", true, { current: current(), next_starts_at: null })), true);
  assert.equal(inMaintenance(device("b", true, { current: null, next_starts_at: NOW + HOUR })), false, "an upcoming one doesn't");
  assert.equal(inMaintenance(device("c", false)), false);
});

test("the wall filters devices in maintenance, online or not", () => {
  const agents = [
    device("held-online", true, { current: current(), next_starts_at: null }),
    device("held-offline", false, { current: current(), next_starts_at: null }),
    device("booked", true, { current: null, next_starts_at: NOW + HOUR }),
    device("plain", false),
  ];
  const filters = { ...DEFAULT_DEVICE_FILTERS, status: "maintenance" };
  assert.deepEqual(filterAgents(agents, filters).map((agent) => agent.id), ["held-online", "held-offline"]);
  assert.equal(serializeDeviceFilters(filters), "?status=maintenance");
  assert.equal(parseDeviceFilters(new URLSearchParams("status=maintenance")).status, "maintenance");
});

test("the device store counts the devices in maintenance as they change", () => {
  const devices = new DeviceStore({ schedule: () => {} });
  const held = { current: current(), next_starts_at: null };
  devices.replace({ agents: [device("a", true, held), device("b", false)], companies: [], tags: [] });
  assert.deepEqual(devices.counts(), { total: 2, online: 1, maintenance: 1 });
  devices.apply({ type: "agent_upsert", revision: 1, agent: device("b", false, held) });
  assert.equal(devices.counts().maintenance, 2);
  devices.apply({ type: "agent_upsert", revision: 2, agent: device("a", true, null) });
  assert.equal(devices.counts().maintenance, 1, "its window ended");
  devices.apply({ type: "agent_deleted", revision: 3, agent_id: "b" });
  assert.deepEqual(devices.counts(), { total: 1, online: 1, maintenance: 0 });
});

test("a start needs a reason and a sensible time, and asks for what was chosen", () => {
  const draft = { ...startDraft(), reason: "  Moving racks " };
  assert.equal(startDraftProblem({ ...draft, reason: " " }, NOW), "Say why the devices are in maintenance.");
  assert.equal(startDraftProblem({ ...draft, reason: "x".repeat(501) }, NOW), "Keep the reason to 500 characters.");
  assert.equal(startDraftProblem(draft, NOW), null);
  const target = { target: "devices", device_ids: ["a"] };
  assert.deepEqual(startBody(draft, target), { target: "devices", device_ids: ["a"], reason: "Moving racks", automation: "skip", duration_minutes: 60 });

  const later = { ...draft, later: true, startsAt: localInput(NOW + HOUR), length: "until", endsAt: localInput(NOW + 4 * HOUR), automation: "scans" };
  assert.equal(startDraftProblem(later, NOW), null);
  assert.deepEqual(startBody(later, { target: "all" }), { target: "all", reason: "Moving racks", automation: "scans", starts_at: NOW + HOUR, ends_at: NOW + 4 * HOUR });
  assert.equal(startDraftProblem({ ...later, startsAt: localInput(NOW - HOUR) }, NOW), "Choose a start in the future, or start it now.");
  assert.equal(startDraftProblem({ ...later, endsAt: localInput(NOW + HOUR) }, NOW), "It must end after it starts.");
  assert.equal(startDraftProblem({ ...later, startsAt: "" }, NOW), "Choose when it starts.");
  const open = { ...draft, length: "open" };
  assert.deepEqual(startBody(open, { target: "companies", company_ids: ["c"] }), { target: "companies", company_ids: ["c"], reason: "Moving racks", automation: "skip" });
});

test("date and time inputs read in the browser's time zone", () => {
  assert.equal(parseLocal(localInput(NOW)), NOW);
  for (const text of ["", "2026-06-07", "2026-06-07 10:00", "tomorrow"]) assert.equal(parseLocal(text), null, text);
});

test("windows say who set and ended them, and lists keep what they show", () => {
  assert.equal(windowSetBy({ created_by: "Ada", ended_by: null, state: "current" }), "Set by Ada");
  assert.equal(windowSetBy({ created_by: "Ada", ended_by: "Bob", state: "ended" }), "Set by Ada; ended by Bob");
  assert.equal(windowSetBy({ created_by: "policy “Nightly”", ended_by: "Bob", state: "cancelled" }), "Set by policy “Nightly”; cancelled by Bob");
  const windows = [{ id: "1", state: "current" }, { id: "2", state: "upcoming" }];
  assert.deepEqual(replaceWindow(windows, { id: "1", state: "ended" }, "open").map((window) => window.id), ["2"], "an ended one leaves the open list");
  assert.deepEqual(replaceWindow(windows, { id: "2", state: "current" }, "open"), [{ id: "1", state: "current" }, { id: "2", state: "current" }]);
  assert.equal(windowsPath("current", { device: "d1" }), "/v1/maintenance-windows?state=current&device=d1");
});

test("a maintenance policy sets windows of a length on a schedule", () => {
  const draft = { ...policyDraft(null, "UTC", "maintenance"), name: "Sunday maintenance", trigger: "weekly", weekdays: [7], time: "02:00", maintenanceMinutes: "180", maintenanceAutomation: "scans" };
  assert.equal(policyDraftProblem(draft), null);
  const body = policyInput(draft);
  assert.equal(body.action, "maintenance");
  assert.equal(body.maintenance_minutes, 180);
  assert.equal(body.maintenance_reason, "");
  assert.equal(body.maintenance_automation, "scans");
  assert.equal(body.offline, "skip", "a window covers offline devices too");
  assert.match(policyDraftProblem({ ...draft, maintenanceMinutes: "2" }), /lasts 5 minutes to 7 days/u);
  assert.equal(policyDraftProblem({ ...draft, trigger: "enroll" }), "A maintenance policy runs on a schedule.");
  assert.equal(describePolicyAction({ action: "maintenance", script_name: null, update_categories: null, maintenance_minutes: 180 }), "Maintenance for 3 hours");
  assert.equal(minutesText(90), "90 minutes");
  assert.equal(minutesText(24 * 60), "1 day");

  const install = { ...policyDraft(null, "UTC", "update_install"), name: "Patch night", maintenanceDuringInstall: true };
  assert.equal(policyInput(install).maintenance_during_install, true);
  assert.equal(policyInput({ ...draft, action: "script", scriptId: "s" }).maintenance_minutes, undefined);
});

test("the maintenance page sits with the device groups, for those who see devices", () => {
  const viewer = { permissions: ["devices.view"], is_administrator: false };
  assert.equal(viewForPath("/maintenance"), "maintenance");
  assert.equal(navViewFor("maintenance"), "companies");
  assert.equal(canView(viewer, "maintenance"), true);
  assert.equal(canView({ permissions: ["scripts.run"], is_administrator: false }, "maintenance"), false);
});
