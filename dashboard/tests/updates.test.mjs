import assert from "node:assert/strict";
import test from "node:test";
import {
  applyFleetAnswer,
  applyRunUpdate,
  categoryCounts,
  chooseDevices,
  countsRefreshDelay,
  describeReboot,
  describeSelection,
  fleetFilterParams,
  fleetKey,
  fleetPagePath,
  formatAge,
  formatDelay,
  installBody,
  installName,
  isStale,
  parseFleetFilters,
  parseUpdateEvent,
  patchFleetRow,
  rebootDelayProblem,
  refreshChosen,
  runRebootText,
  runSummary,
  runTone,
  sortPendingUpdates,
  updateDetail,
} from "../features/updates/model.ts";
import { describePolicyAction, policyDraft, policyDraftProblem, policyInput } from "../features/automation/model.ts";
import { clearsItself, ruleForm, ruleFormProblem, ruleInput, withCondition } from "../features/alerts/model.ts";
import { canView, viewForPath } from "../features/workspace/views.ts";

const DAY = 24 * 60 * 60_000;
const NOW = 1_800_000_000_000;

const update = (id, category, change = {}) => ({
  id,
  title: id,
  kb: null,
  version: null,
  current_version: null,
  category,
  severity: null,
  size_bytes: null,
  reboot_required: false,
  released_at: null,
  source: null,
  first_seen_at: NOW - DAY,
  ...change,
});

const summary = (id, change = {}) => ({
  device_id: id,
  device_name: id,
  company_id: null,
  platform: "linux",
  connected: true,
  manager: "apt",
  supported: true,
  can_select: true,
  scanned_at: NOW - 60_000,
  received_at: NOW - 60_000,
  scan_error: null,
  reboot_pending: false,
  reboot_pending_since: null,
  pending: 0,
  security: 0,
  critical: 0,
  oldest_pending_at: null,
  scanning: false,
  installing: false,
  last_install: null,
  ...change,
});

const run = (id, change = {}) => ({
  id,
  device_id: "d",
  device_name: "D",
  kind: "install",
  status: "succeeded",
  progress: null,
  progress_percent: null,
  update_ids: [],
  categories: [],
  reboot: "if_required",
  reboot_delay_minutes: 5,
  requested_by: "Ada",
  policy_name: null,
  job_id: null,
  error: null,
  installed: 0,
  failed: 0,
  reboot_required: false,
  reboot_scheduled_at: null,
  reboot_error: null,
  created_at: 1,
  completed_at: 2,
  ...change,
});

test("pending updates list the most urgent first, and count by category", () => {
  const updates = [
    update("zlib", "other"),
    update("kernel", "security", { severity: "moderate" }),
    update("openssl", "security", { severity: "critical" }),
    update("defender", "definition"),
  ];
  assert.deepEqual(sortPendingUpdates(updates).map((entry) => entry.id), ["openssl", "kernel", "defender", "zlib"]);
  assert.deepEqual(categoryCounts(updates), [
    { category: "security", count: 2 },
    { category: "definition", count: 1 },
    { category: "other", count: 1 },
  ]);
  assert.equal(updateDetail({ kb: "KB5034441", version: null, current_version: null, source: null }), "KB5034441");
  assert.equal(updateDetail({ kb: null, version: "3.0.14", current_version: "3.0.13", source: "noble-security" }), "3.0.13 → 3.0.14 · noble-security");
});

test("ages, delays, selections and restarts read in words", () => {
  assert.equal(formatAge(NOW - 30_000, NOW), "1 minute");
  assert.equal(formatAge(NOW - 5 * 60_000, NOW), "5 minutes");
  assert.equal(formatAge(NOW - 3 * 60 * 60_000, NOW), "3 hours");
  assert.equal(formatAge(NOW - 9 * DAY, NOW), "9 days");
  assert.equal(formatDelay(1), "1 minute");
  assert.equal(formatDelay(90), "1.5 hours");
  assert.equal(formatDelay(1440), "1 day");
  assert.equal(describeSelection({}), "All updates");
  assert.equal(describeSelection({ categories: ["critical", "security"] }), "Security and critical updates");
  assert.equal(describeSelection({ categories: ["security", "critical", "definition"] }), "Security, critical and definition updates");
  assert.equal(describeSelection({ update_ids: ["a", "b"], categories: ["security"] }), "2 chosen updates");
  assert.equal(installName([]), "Install all updates");
  assert.equal(installName(["security"]), "Install security updates");
  assert.equal(describeReboot("never", 5), "Doesn’t restart");
  assert.equal(describeReboot("if_required", 5), "Restarts if needed, 5 minutes after warning the user");
  assert.equal(describeReboot("always", 60), "Restarts, 1 hour after warning the user");
  assert.equal(describeReboot(null, null), "Doesn’t restart");
});

test("runs summarize their result", () => {
  assert.equal(runSummary(run("a", { installed: 3, failed: 1, status: "partial" })), "3 installed · 1 failed");
  assert.equal(runSummary(run("a", { status: "pending" })), "Installing");
  assert.equal(runSummary(run("a", { status: "lost" })), "The device never reported back");
  assert.equal(runSummary(run("a", { status: "failed", error: "dpkg is locked" })), "dpkg is locked");
  assert.equal(runSummary(run("a")), "Nothing was pending");
  assert.equal(runSummary(run("a", { kind: "scan", status: "succeeded" })), "Scanned");
  assert.equal(runTone("partial"), "problem");
  assert.equal(runTone("lost"), "muted");
  const time = (value) => `t${value}`;
  assert.equal(runRebootText(run("a", { reboot_scheduled_at: 9 }), time), "Restart set for t9");
  assert.equal(runRebootText(run("a", { reboot_error: "denied" }), time), "Couldn’t restart: denied");
  assert.equal(runRebootText(run("a", { reboot_required: true }), time), "Needs a restart to finish");
  assert.equal(runRebootText(run("a", { status: "pending", reboot_required: true }), time), null);
});

test("installs take the chosen updates, or categories, and a valid delay", () => {
  assert.deepEqual(installBody({ updateIds: ["a"], categories: ["security"], reboot: "never", rebootDelayMinutes: 5 }), {
    update_ids: ["a"], categories: [], reboot: "never", reboot_delay_minutes: 5,
  });
  assert.deepEqual(installBody({ updateIds: [], categories: ["security"], reboot: "always", rebootDelayMinutes: 15 }), {
    update_ids: [], categories: ["security"], reboot: "always", reboot_delay_minutes: 15,
  });
  assert.equal(rebootDelayProblem(5), null);
  assert.equal(rebootDelayProblem(1440), null);
  assert.ok(rebootDelayProblem(0));
  assert.ok(rebootDelayProblem(1441));
  assert.ok(rebootDelayProblem(2.5));
});

test("a device's last scan is old after three days, unless it has no update manager", () => {
  assert.equal(isStale(summary("web"), NOW), false);
  assert.equal(isStale(summary("old", { received_at: NOW - 3 * DAY - 1 }), NOW), true);
  assert.equal(isStale(summary("new", { received_at: null }), NOW), true);
  assert.equal(isStale(summary("zypper", { supported: false, received_at: null }), NOW), false);
});

const NO_FILTERS = { query: "", company: "", tag: "", platform: "", status: "" };
const TOTALS = { devices: 3, pending: 1, security: 1, failed: 0, reboot: 0, stale: 0 };
const page = (names, change = {}) => ({ devices: names.map((name) => summary(name)), matching: names.length, totals: TOTALS, ...change });

test("the fleet is asked for a page at a time with the filters", () => {
  assert.equal(fleetPagePath(NO_FILTERS, 0), "/v1/updates/devices?limit=100");
  assert.equal(
    fleetPagePath({ query: " web 1 ", company: "none", tag: "t1", platform: "linux", status: "stale" }, 200),
    "/v1/updates/devices?query=web+1&company=none&tag=t1&platform=linux&status=stale&limit=100&offset=200",
  );
  assert.equal(fleetPagePath({ ...NO_FILTERS, status: "failed" }, 0, 0), "/v1/updates/devices?status=failed&limit=0", "the counts alone");
  assert.equal(fleetKey({ ...NO_FILTERS, query: "web " }), fleetKey({ ...NO_FILTERS, query: " web" }));
  assert.notEqual(fleetKey(NO_FILTERS), fleetKey({ ...NO_FILTERS, tag: "t1" }));
});

test("further pages join the list they were asked for", () => {
  const all = fleetKey(NO_FILTERS);
  const first = applyFleetAnswer(null, { kind: "page", key: all, ticket: 1, offset: 0, page: page(["a", "b"], { matching: 5 }) }, all);
  assert.deepEqual(first.items.map((device) => device.device_id), ["a", "b"]);
  assert.equal(first.matching, 5);
  assert.deepEqual(first.totals, TOTALS);
  // The fleet moved between pages: "b" comes again, and stays where it was.
  const second = applyFleetAnswer(first, { kind: "page", key: all, ticket: 2, offset: 2, page: page(["b", "c", "d"], { matching: 6 }) }, all);
  assert.deepEqual(second.items.map((device) => device.device_id), ["a", "b", "c", "d"]);
  assert.equal(second.matching, 6);
  // A page of other filters doesn't join, nor one asked for before the
  // list was loaded again.
  const other = fleetKey({ ...NO_FILTERS, status: "pending" });
  assert.equal(applyFleetAnswer(second, { kind: "page", key: other, ticket: 3, offset: 4, page: page(["x"]) }, all), second);
  const reloaded = applyFleetAnswer(second, { kind: "page", key: all, ticket: 5, offset: 0, page: page(["d", "a"]) }, all);
  assert.deepEqual(reloaded.items.map((device) => device.device_id), ["d", "a"], "a reload starts over");
  assert.equal(applyFleetAnswer(reloaded, { kind: "page", key: all, ticket: 4, offset: 4, page: page(["y"]) }, all), reloaded);
  // New filters start a new list.
  const narrowed = applyFleetAnswer(reloaded, { kind: "page", key: other, ticket: 6, offset: 0, page: page(["c"]) }, other);
  assert.equal(narrowed.key, other);
  assert.deepEqual(narrowed.items.map((device) => device.device_id), ["c"]);
});

test("a newer request's answer always wins over an older one's", () => {
  const all = fleetKey(NO_FILTERS);
  const security = fleetKey({ ...NO_FILTERS, status: "security" });
  // Filters changed while the first answer was on its way: only the
  // answer for the filters wanted now is shown.
  assert.equal(applyFleetAnswer(null, { kind: "page", key: all, ticket: 1, offset: 0, page: page(["a", "b"]) }, security), null);
  const shown = applyFleetAnswer(null, { kind: "page", key: security, ticket: 2, offset: 0, page: page(["a"]) }, security);
  assert.equal(applyFleetAnswer(shown, { kind: "page", key: all, ticket: 1, offset: 0, page: page(["a", "b"]) }, security), shown);
  // Two loads of the same list: the older answer, arriving last, is dropped.
  const newer = applyFleetAnswer(shown, { kind: "page", key: security, ticket: 4, offset: 0, page: page(["b"]) }, security);
  assert.equal(applyFleetAnswer(newer, { kind: "page", key: security, ticket: 3, offset: 0, page: page(["a"]) }, security), newer);
  // Counts: the newest shown wins; the fleet's totals come from any
  // filters, how many match only from the list's own.
  const counted = applyFleetAnswer(newer, { kind: "counts", key: security, ticket: 6, page: page([], { matching: 7, totals: { ...TOTALS, devices: 9 } }) }, security);
  assert.equal(counted.matching, 7);
  assert.equal(counted.totals.devices, 9);
  assert.deepEqual(counted.items, newer.items, "counts leave the rows");
  assert.equal(applyFleetAnswer(counted, { kind: "counts", key: security, ticket: 5, page: page([], { matching: 1, totals: TOTALS }) }, security), counted);
  const elsewhere = applyFleetAnswer(counted, { kind: "counts", key: all, ticket: 7, page: page([], { matching: 2, totals: { ...TOTALS, devices: 10 } }) }, security);
  assert.equal(elsewhere.matching, 7);
  assert.equal(elsewhere.totals.devices, 10);
  // A page asked for before the newest counts keeps them.
  const more = applyFleetAnswer(elsewhere, { kind: "page", key: security, ticket: 6, offset: 1, page: page(["c"], { matching: 3 }) }, security);
  assert.deepEqual(more.items.map((device) => device.device_id), ["b", "c"]);
  assert.equal(more.totals.devices, 10);
  assert.equal(more.matching, 7);
});

test("events patch loaded rows in place and the counts are read again at most every 10 seconds", () => {
  const all = fleetKey(NO_FILTERS);
  const list = applyFleetAnswer(null, { kind: "page", key: all, ticket: 1, offset: 0, page: page(["a", "b"]) }, all);
  const patched = patchFleetRow(list, summary("b", { pending: 4, installing: true }));
  assert.deepEqual(patched.items.map((device) => [device.device_id, device.pending]), [["a", 0], ["b", 4]]);
  assert.equal(patched.items[1].installing, true);
  assert.equal(patchFleetRow(list, summary("unloaded", { pending: 9 })), list, "a device not loaded stays out");
  assert.equal(patchFleetRow(null, summary("a")), null);

  assert.equal(countsRefreshDelay(0, NOW), 0, "the first change after a quiet while");
  assert.equal(countsRefreshDelay(NOW - 4_000, NOW), 6_000);
  assert.equal(countsRefreshDelay(NOW - 10_000, NOW), 0);
});

test("chosen devices outlive pages and filters, with their newest summaries", () => {
  const chosen = chooseDevices(new Map(), [summary("a"), summary("b")], true);
  assert.deepEqual([...chosen.keys()], ["a", "b"]);
  assert.deepEqual([...chooseDevices(chosen, [summary("a")], false).keys()], ["b"]);
  const refreshed = refreshChosen(chosen, [summary("b", { pending: 3 }), summary("z", { pending: 1 })]);
  assert.equal(refreshed.get("b").pending, 3);
  assert.equal(refreshed.has("z"), false, "refreshing chooses nothing new");
  assert.equal(refreshChosen(chosen, [summary("z")]), chosen);
});

test("fleet filters round trip through the address", () => {
  const filters = { query: " web ", company: "acme", tag: "t1", platform: "linux", status: "reboot" };
  const params = fleetFilterParams(filters);
  assert.equal(params.toString(), "q=web&company=acme&tag=t1&os=linux&status=reboot");
  assert.deepEqual(parseFleetFilters(params), { ...filters, query: "web" });
  assert.deepEqual(parseFleetFilters(new URLSearchParams("os=beos&status=nope")), { query: "", company: "", tag: "", platform: "", status: "" });
});

test("socket events update the fleet and the history", () => {
  assert.deepEqual(parseUpdateEvent({ type: "updates_resync" }), { type: "updates_resync" });
  const device = summary("web", { pending: 2 });
  assert.deepEqual(parseUpdateEvent({ type: "device_updates", device }), { type: "device_updates", device });
  assert.equal(parseUpdateEvent({ type: "device_updates", device: { id: "x" } }), null);
  assert.equal(parseUpdateEvent({ type: "job_update" }), null);
  const changed = run("b", { status: "pending" });
  assert.deepEqual(parseUpdateEvent({ type: "update_run", run: changed }), { type: "update_run", run: changed });


  const items = [{ update_id: "a", title: "A", kb: null, category: "other", outcome: "installed", error: null }];
  const history = [run("a", { created_at: 1, items }), run("b", { created_at: 2, status: "pending" })];
  const next = applyRunUpdate(history, run("a", { created_at: 1, installed: 1 }));
  assert.deepEqual(next.map((entry) => entry.id), ["b", "a"]);
  assert.deepEqual(next[1].items, items, "an event keeps the updates the history had");
  assert.equal(next[1].installed, 1);
  assert.deepEqual(applyRunUpdate(history, run("c", { created_at: 3 })).map((entry) => entry.id), ["c", "b", "a"]);
});

test("update policies save their action, categories and restart", () => {
  const draft = { ...policyDraft(null, "UTC", "update_install"), name: "Patch Tuesday" };
  assert.equal(draft.action, "update_install");
  assert.deepEqual(draft.updateCategories, ["security", "critical"]);
  assert.equal(policyDraftProblem(draft), null, "an update policy needs no script");
  const body = policyInput(draft);
  assert.equal(body.action, "update_install");
  assert.deepEqual(body.update_categories, ["security", "critical"]);
  assert.equal(body.reboot, "if_required");
  assert.equal(body.reboot_delay_minutes, 5);
  assert.equal(body.script_id, undefined);
  assert.ok(policyDraftProblem({ ...draft, rebootDelayMinutes: 0 }));
  assert.equal(policyDraftProblem({ ...draft, reboot: "never", rebootDelayMinutes: 0 }), null);

  const scan = policyInput({ ...draft, action: "update_scan" });
  assert.equal(scan.action, "update_scan");
  assert.equal(scan.update_categories, undefined);
  assert.equal(scan.reboot, undefined);

  const script = { ...policyDraft(null, "UTC"), name: "Nightly" };
  assert.equal(script.action, "script");
  assert.ok(policyDraftProblem(script), "a script policy needs its script");
  assert.equal(policyInput({ ...script, scriptId: "s" }).script_id, "s");

  const saved = { action: "update_install", update_categories: [], reboot: "always", reboot_delay_minutes: 30, script_id: null, run_as: null };
  const edited = policyDraft({ ...saved, name: "All", enabled: true, target: "all", device_ids: [], company_ids: [], trigger: "daily", time: "03:00", time_zone: "UTC", offline: "wait", catch_up_hours: 24, channel_ids: [] }, "UTC");
  assert.deepEqual(edited.updateCategories, [], "a saved policy for every update stays that way");
  assert.equal(edited.reboot, "always");
  assert.equal(describePolicyAction({ action: "update_install", update_categories: [], script_name: null }), "Install all updates");
  assert.equal(describePolicyAction({ action: "update_scan", script_name: null }), "Scan for updates");
  assert.equal(describePolicyAction({ action: "script", script_name: null }), "Deleted script");
});

test("update alert rules take a category and days", () => {
  const form = { ...withCondition(ruleForm(null), "updates_pending"), name: "Security backlog" };
  assert.equal(form.days, "7");
  assert.equal(ruleFormProblem(form), null);
  assert.deepEqual(
    (({ update_category, days, threshold, duration_minutes }) => ({ update_category, days, threshold, duration_minutes }))(ruleInput({ ...form, updateCategory: "any" })),
    { update_category: "any", days: 7, threshold: null, duration_minutes: null },
  );
  assert.ok(ruleFormProblem({ ...form, days: "0" }));
  assert.ok(ruleFormProblem({ ...form, days: "366" }));
  const reboot = { ...withCondition(form, "reboot_pending"), name: "Restart" };
  assert.equal(reboot.days, "3");
  assert.deepEqual([ruleInput(reboot).update_category, ruleInput(reboot).days], [null, 3]);
  const failed = { ...withCondition(form, "update_install_failed"), name: "Failed" };
  assert.equal(ruleFormProblem(failed), null);
  assert.deepEqual([ruleInput(failed).update_category, ruleInput(failed).days], [null, null]);
  assert.equal(clearsItself("update_install_failed"), false);
  assert.equal(clearsItself("updates_pending"), true);
  const loaded = ruleForm({ name: "R", severity: "warning", enabled: true, condition: "updates_pending", threshold: null, threshold_unit: null, duration_minutes: null, nonzero_exit_fails: false, update_category: "critical", days: 14, target: "all", company_ids: [], tag_ids: [], tag_match: "any", channel_ids: [], notify_on_resolve: false });
  assert.deepEqual([loaded.updateCategory, loaded.days], ["critical", "14"]);
});

test("the Updates page opens to who may see updates", () => {
  const account = (permissions) => ({ permissions, is_administrator: false });
  assert.equal(viewForPath("/updates"), "updates");
  assert.equal(canView(account(["devices.view", "updates.view"]), "updates"), true);
  assert.equal(canView(account(["devices.view"]), "updates"), false);
  assert.equal(canView(account(["updates.install"]), "jobs"), true, "who installs on many devices sees those jobs");
});
