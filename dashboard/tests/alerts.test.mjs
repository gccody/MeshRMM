import assert from "node:assert/strict";
import test from "node:test";
import { reloadGate } from "../lib/reload-gate.ts";
import {
  ALERT_PAGE_LIMIT,
  DEFAULT_ALERT_FILTERS,
  alertEventEffect,
  alertFilterParams,
  applyAlertChanges,
  channelForm,
  channelFormProblem,
  channelInput,
  matchesAlertFilters,
  needsRefill,
  parseAlertEvent,
  parseAlertFilters,
  resolutionLabel,
  ruleForm,
  ruleFormProblem,
  ruleInput,
  scopeLabel,
  sortAlerts,
  topSeverity,
  withCondition,
} from "../features/alerts/model.ts";

const counts = { open: 2, acknowledged: 1, open_info: 1, open_warning: 0, open_critical: 1 };

const alert = (id, change = {}) => ({
  id,
  rule_id: "r",
  rule_name: "Rule",
  severity: "warning",
  condition: "cpu",
  device_id: "d1",
  device_name: "Desk",
  company_id: null,
  company_name: null,
  subject: "",
  state: "open",
  value: 95,
  detail: "CPU averaged 95% over 5 minutes",
  script_run_id: null,
  occurrences: 1,
  raised_at: 100,
  last_occurred_at: 100,
  acknowledged_at: null,
  acknowledged_by: null,
  resolved_at: null,
  resolved_by: null,
  resolution: null,
  ...change,
});

test("alert events are told apart from other socket messages", () => {
  assert.deepEqual(parseAlertEvent({ type: "alert_counts", counts }), { type: "alert_counts", counts });
  const upsert = { type: "alert_upsert", alert: alert("a"), counts };
  assert.deepEqual(parseAlertEvent(upsert), upsert);
  assert.equal(parseAlertEvent({ type: "alert_counts", counts: { open: 1 } }), null);
  assert.equal(parseAlertEvent({ type: "alert_upsert", alert: { id: "a" }, counts }), null);
  assert.equal(parseAlertEvent({ type: "snapshot" }), null);
  assert.equal(parseAlertEvent(null), null);
});

test("the navigation's count takes the most severe open alert's color", () => {
  assert.equal(topSeverity(counts), "critical");
  assert.equal(topSeverity({ ...counts, open_critical: 0 }), "info");
  assert.equal(topSeverity({ ...counts, open: 0, open_info: 0, open_critical: 0 }), null);
});

test("filters round-trip through the search string, defaults left out", () => {
  assert.deepEqual(parseAlertFilters(new URLSearchParams("")), DEFAULT_ALERT_FILTERS);
  const filters = parseAlertFilters(new URLSearchParams("state=resolved&severity=critical&company=none&device=d1"));
  assert.deepEqual(filters, { state: "resolved", severity: "critical", company: "none", device: "d1" });
  assert.equal(alertFilterParams(filters).toString(), "state=resolved&severity=critical&company=none&device=d1");
  assert.equal(alertFilterParams(DEFAULT_ALERT_FILTERS).toString(), "");
  assert.deepEqual(parseAlertFilters(new URLSearchParams("state=bogus&severity=loud")), DEFAULT_ALERT_FILTERS);
});

test("lists keep open alerts first and follow changes within their filters", () => {
  const active = DEFAULT_ALERT_FILTERS;
  assert.equal(matchesAlertFilters(alert("a"), active), true);
  assert.equal(matchesAlertFilters(alert("a", { state: "resolved" }), active), false);
  assert.equal(matchesAlertFilters(alert("a", { company_id: "c" }), { ...active, company: "none" }), false);
  assert.equal(matchesAlertFilters(alert("a"), { ...active, company: "none" }), true);
  assert.equal(matchesAlertFilters(alert("a"), { ...active, severity: "critical" }), false);

  const sorted = sortAlerts([
    alert("old-open", { raised_at: 1 }),
    alert("acked", { state: "acknowledged", raised_at: 50 }),
    alert("new-open", { raised_at: 9 }),
    alert("resolved", { state: "resolved", raised_at: 99 }),
  ]);
  assert.deepEqual(sorted.map(({ id }) => id), ["new-open", "old-open", "acked", "resolved"]);
});

// The order the server lists alerts in, checked pairwise.
const assertSorted = (alerts) => {
  for (let i = 1; i < alerts.length; i++) {
    assert.deepEqual(sortAlerts([alerts[i], alerts[i - 1]]).map(({ id }) => id), [alerts[i - 1].id, alerts[i].id], `rows ${i - 1} and ${i}`);
  }
};

// The server's first `count` open alerts, newest first, raised at `top`,
// `top - 1`, ...
const page = (count, top, truncated) => ({
  alerts: Array.from({ length: count }, (_, index) => alert(`p${index}`, { raised_at: top - index })),
  counts,
  truncated,
});

const ids = (list) => list.alerts.map(({ id }) => id);

test("an alert storm keeps a full list at its limit, sorted, and says more match", () => {
  const active = DEFAULT_ALERT_FILTERS;
  let list = page(ALERT_PAGE_LIMIT, 10_000, true);
  // A site goes offline: thousands of alerts are raised, a frame's worth at
  // a time, each newer than any loaded, while loaded ones resolve or are
  // acknowledged.
  let raised = 0;
  for (let frame = 0; frame < 250; frame++) {
    const batch = [];
    for (let i = 0; i < 20; i++) batch.push(alert(`storm${raised}`, { raised_at: 20_000 + raised++ }));
    const loaded = frame % ALERT_PAGE_LIMIT;
    batch.push(alert(`p${loaded}`, { raised_at: 10_000 - loaded, state: frame % 2 ? "resolved" : "acknowledged" }));
    list = applyAlertChanges(list, batch, active, ALERT_PAGE_LIMIT);
    assert.ok(list.alerts.length <= ALERT_PAGE_LIMIT, `frame ${frame} holds ${list.alerts.length}`);
  }
  assert.equal(list.alerts.length, ALERT_PAGE_LIMIT);
  assert.equal(list.truncated, true, "the page says more match");
  assertSorted(list.alerts);
  assert.deepEqual(ids(list), Array.from({ length: ALERT_PAGE_LIMIT }, (_, i) => `storm${4_999 - i}`), "the newest of the 5,000 raised");
  assert.equal(list.counts, counts, "the rest of the answer is kept");

  // The whole storm in one batch, into a short complete list, ends the same.
  const storm = Array.from({ length: 5_000 }, (_, i) => alert(`storm${i}`, { raised_at: 20_000 + i }));
  const once = applyAlertChanges(page(3, 100, false), storm, active, ALERT_PAGE_LIMIT);
  assert.equal(once.alerts.length, ALERT_PAGE_LIMIT);
  assert.equal(once.truncated, true, "a complete list that overflows says more match");
  assert.deepEqual(ids(once), ids(list));
});

test("a truncated list takes only changes that belong among its loaded alerts", () => {
  const active = DEFAULT_ALERT_FILTERS;
  // p0..p4, raised at 100..96; more match after p4.
  const list = page(5, 100, true);

  const between = applyAlertChanges(list, [alert("n", { raised_at: 98.5 })], active, 5);
  assert.deepEqual(ids(between), ["p0", "p1", "n", "p2", "p3"], "in its sorted place, pushing the last out");
  assert.equal(between.truncated, true);

  const older = applyAlertChanges(list, [alert("old", { raised_at: 1 })], active, 5);
  assert.equal(older, list, "an alert past the loaded ones may have others before it, so it stays out");

  const acknowledged = applyAlertChanges(list, [alert("p1", { raised_at: 99, state: "acknowledged" })], active, 5);
  assert.deepEqual(ids(acknowledged), ["p0", "p2", "p3", "p4"], "an alert that moves past the loaded ones leaves");
  assert.equal(needsRefill(acknowledged, 5), true, "and the list should be read again to fill it");
  assert.equal(needsRefill(list, 5), false);

  const resolved = applyAlertChanges(list, [alert("p2", { raised_at: 98, state: "resolved" })], active, 5);
  assert.deepEqual(ids(resolved), ["p0", "p1", "p3", "p4"], "an alert that leaves the filters leaves the list");

  const severity = applyAlertChanges(list, [alert("n", { raised_at: 200, severity: "critical" })], { ...active, severity: "warning" }, 5);
  assert.equal(severity, list, "an alert outside the filters stays out");

  const updated = applyAlertChanges(list, [alert("p3", { raised_at: 97, occurrences: 4 })], active, 5);
  assert.deepEqual(ids(updated), ids(list), "a changed alert is replaced where it is");
  assert.equal(updated.alerts[3].occurrences, 4);

  const twice = applyAlertChanges(list, [alert("p0", { state: "acknowledged" }), alert("p0", { occurrences: 9 })], active, 5);
  assert.deepEqual(ids(twice), ids(list), "the last change of an alert counts");
  assert.equal(twice.alerts[0].occurrences, 9);

  const emptied = applyAlertChanges(list, list.alerts.map((row) => ({ ...row, state: "resolved" })), active, 5);
  assert.deepEqual(emptied.alerts, []);
  assert.equal(applyAlertChanges(emptied, [alert("n", { raised_at: 500 })], active, 5), emptied, "an emptied truncated list waits to be read again");
  assert.equal(needsRefill(emptied, 5), true);
});

test("a complete list takes every matching change, up to its limit", () => {
  const active = DEFAULT_ALERT_FILTERS;
  const list = page(2, 100, false);
  const older = applyAlertChanges(list, [alert("old", { raised_at: 1 })], active, 3);
  assert.deepEqual(ids(older), ["p0", "p1", "old"]);
  assert.equal(older.truncated, false);
  const full = applyAlertChanges(older, [alert("older", { raised_at: 0 })], active, 3);
  assert.deepEqual(ids(full), ["p0", "p1", "old"], "past the limit, the last are dropped");
  assert.equal(full.truncated, true);
  assert.equal(applyAlertChanges(list, [], active, 3), list, "no changes, the same list");
});

test("resolutions say who or what resolved the alert", () => {
  assert.equal(resolutionLabel({ resolution: "manual", resolved_by: "ada@example.com" }), "Resolved by ada@example.com");
  assert.equal(resolutionLabel({ resolution: "cleared", resolved_by: null }), "Resolved itself: the condition passed");
  assert.equal(resolutionLabel({ resolution: "not_applicable", resolved_by: null }), "Resolved: the rule no longer covers it");
});

test("the rule form sends only what its condition uses", () => {
  const form = ruleForm(null);
  assert.equal(form.condition, "device_offline");
  assert.equal(ruleFormProblem(form), "Name the rule.");
  const offline = { ...form, name: " Offline " };
  assert.equal(ruleFormProblem(offline), null);
  assert.deepEqual(ruleInput(offline), {
    name: "Offline",
    severity: "warning",
    enabled: true,
    condition: "device_offline",
    threshold: null,
    threshold_unit: null,
    duration_minutes: 15,
    nonzero_exit_fails: false,
    update_category: null,
    days: null,
    target: "all",
    company_ids: [],
    tag_ids: [],
    tag_match: "any",
    channel_ids: [],
    notify_on_resolve: false,
    monitor: null,
    remediation: null,
    notify_when: "raised",
  });

  const disk = withCondition(offline, "disk_space");
  assert.equal(disk.threshold, "10");
  assert.equal(ruleInput(disk).threshold, 10);
  assert.equal(ruleInput(disk).threshold_unit, "percent");
  assert.equal(ruleInput(disk).duration_minutes, null);
  assert.equal(ruleFormProblem({ ...disk, threshold: "100" }), "Enter a percentage above 0 and below 100.");
  assert.equal(ruleFormProblem({ ...disk, threshold: "250", thresholdUnit: "gb" }), null);

  const cpu = withCondition(offline, "cpu");
  assert.equal(ruleFormProblem({ ...cpu, duration: "61" }), "Enter 1 to 60 minutes.");
  assert.equal(ruleFormProblem({ ...cpu, duration: "1.5" }), "Enter 1 to 60 minutes.");
  assert.equal(ruleFormProblem({ ...cpu, target: "companies" }), "Choose at least one company, or every device.");
  assert.equal(ruleFormProblem({ ...cpu, target: "tags" }), "Choose at least one tag, or every device.");
  assert.deepEqual(ruleInput({ ...cpu, target: "companies", companyIds: ["c"] }).company_ids, ["c"]);
  assert.deepEqual(ruleInput({ ...cpu, target: "all", companyIds: ["c"], tagIds: ["t"] }).company_ids, []);
  const tagged = ruleInput({ ...cpu, target: "tags", companyIds: ["c"], tagIds: ["t"], tagMatch: "all" });
  assert.deepEqual([tagged.target, tagged.company_ids, tagged.tag_ids, tagged.tag_match], ["tags", [], ["t"], "all"]);

  const script = withCondition(offline, "script_failed");
  assert.equal(ruleInput({ ...script, nonzeroExitFails: true }).nonzero_exit_fails, true);
  assert.equal(ruleInput({ ...cpu, nonzeroExitFails: true }).nonzero_exit_fails, false);

  const saved = ruleForm({ ...ruleInput(cpu), id: "r", threshold: 85, duration_minutes: 5, description: "", created_at: 0, updated_at: 0 });
  assert.equal(saved.threshold, "85");
  assert.equal(saved.duration, "5");
});

test("rules name the companies or tags they cover", () => {
  const names = new Map([["a", "Acme"], ["b", "Beta"], ["c", "Gamma"]]);
  assert.equal(scopeLabel({ target: "all", company_ids: [] }, names), "Every device");
  assert.equal(scopeLabel({ target: "companies", company_ids: ["a", "b"] }, names), "Acme, Beta");
  assert.equal(scopeLabel({ target: "companies", company_ids: ["a", "b", "c"] }, names), "Acme, Beta and 1 more");
  assert.equal(scopeLabel({ target: "companies", company_ids: ["gone"] }, names), "No companies");
  const tags = [{ id: "l", name: "London", color: "green" }, { id: "s", name: "Servers", color: "blue" }];
  assert.equal(scopeLabel({ target: "tags", company_ids: [], tag_ids: ["s", "l"], tag_match: "all" }, names, tags), "Tagged London and Servers");
  assert.equal(scopeLabel({ target: "tags", company_ids: [], tag_ids: ["l"], tag_match: "any" }, names, tags), "Tagged London");
  assert.equal(scopeLabel({ target: "tags", company_ids: [], tag_ids: ["gone"], tag_match: "any" }, names, tags), "Deleted tags");
});

test("the channel form keeps, replaces or removes a webhook's secret", () => {
  assert.equal(channelForm(null, false).kind, "webhook", "email can't be chosen without SMTP");
  assert.equal(channelForm(null, true).kind, "email");
  const email = { ...channelForm(null, true), name: "Admins" };
  assert.equal(channelFormProblem(email), "Choose who to email.");
  assert.deepEqual(channelInput({ ...email, roleIds: ["administrator"] }), {
    name: "Admins", kind: "email", enabled: true, user_ids: [], role_ids: ["administrator"],
  });

  const webhook = { ...channelForm(null, true), name: "Hook", kind: "webhook", url: "ftp://x" };
  assert.equal(channelFormProblem(webhook), "The URL must start with https:// or http://.");
  const fresh = { ...webhook, url: " https://hooks.example.com/x " };
  assert.equal(channelFormProblem(fresh), null);
  assert.deepEqual(channelInput(fresh), { name: "Hook", kind: "webhook", enabled: true, webhook_url: "https://hooks.example.com/x", webhook_secret: null });
  assert.equal(channelInput({ ...fresh, secret: "s3cret" }).webhook_secret, "s3cret");

  const saved = channelForm({ id: "c", name: "Hook", kind: "webhook", enabled: true, webhook_url: "https://h/x", has_secret: true, user_ids: [], role_ids: [] }, true);
  assert.equal(saved.secretMode, "keep");
  assert.equal("webhook_secret" in channelInput(saved), false, "a kept secret isn't sent");
  assert.equal(channelInput({ ...saved, secretMode: "remove" }).webhook_secret, null);
});

const upsert = (alert) => ({ type: "alert_upsert", alert, counts });
const deviceFilters = { ...DEFAULT_ALERT_FILTERS, state: "all", device: "d1" };

test("only the counts read a device's list again, and only its own alerts are merged", () => {
  assert.deepEqual(alertEventEffect({ type: "alert_counts", counts }, deviceFilters), { change: null, read: true }, "counts name no device, so any may have changed");
  const other = alert("x", { device_id: "d2" });
  assert.deepEqual(alertEventEffect(upsert(other), deviceFilters), { change: null, read: false }, "another device's alert is ignored");
  const own = alert("a", { device_id: "d1", state: "resolved" });
  assert.deepEqual(alertEventEffect(upsert(own), deviceFilters), { change: own, read: false }, "the device's own alert is merged, not read");
  assert.deepEqual(alertEventEffect(upsert(other), DEFAULT_ALERT_FILTERS), { change: other, read: false }, "the Alerts page with no device takes every alert");
  assert.deepEqual(alertEventEffect({ type: "monitor_update", device_id: "d1", states: [] }, deviceFilters), { change: null, read: false });
});

test("a fleet-wide alert storm leaves a device's list as it was and unread, while its own alerts change it in place", () => {
  // The device's 20 shown alerts: 5 open, raised at 10..6, then 15 resolved,
  // raised at 200..186; more resolved ones match.
  const shown = {
    alerts: [
      ...Array.from({ length: 5 }, (_, i) => alert(`o${i}`, { raised_at: 10 - i })),
      ...Array.from({ length: 15 }, (_, i) => alert(`r${i}`, { raised_at: 200 - i, state: "resolved" })),
    ],
    counts,
    truncated: true,
  };
  let list = shown;
  let reads = 0;
  const handle = (events) => {
    const changes = [];
    for (const event of events) {
      const { change, read } = alertEventEffect(event, deviceFilters);
      if (change) changes.push(change);
      if (read) reads++;
    }
    list = applyAlertChanges(list, changes, deviceFilters, 20);
    if (needsRefill(list, 20)) reads++;
  };

  handle(Array.from({ length: 5_000 }, (_, i) => upsert(alert(`storm${i}`, { device_id: `d${i + 2}`, raised_at: 1_000 + i }))));
  assert.equal(list, shown, "other devices' alerts change nothing");
  assert.equal(reads, 0, "and read nothing");

  handle([upsert(alert("new", { raised_at: 500 }))]);
  assert.deepEqual(ids(list).slice(0, 2), ["new", "o0"], "the device's new alert shows at once, unresolved first");
  assert.equal(list.alerts.length, 20, "at most 20");
  assert.equal(reads, 0);

  handle([upsert(alert("o0", { raised_at: 10, state: "resolved" }))]);
  assert.equal(list.alerts.some(({ id }) => id === "o0"), false, "resolved, it sorts among the resolved alerts never loaded, so it leaves");
  assert.equal(list.alerts.filter(({ state }) => state !== "resolved").length, 5, "the unresolved count drops by the one resolved");
  assert.equal(reads, 1, "and the list is read once to fill the gap");
});

test("a device page reads its list at most once per interval while counts pour in, and nothing while hidden", async (t) => {
  t.mock.timers.enable({ apis: ["setTimeout"] });
  const flush = async () => { for (let i = 0; i < 10; i++) await Promise.resolve(); };
  let hidden = false;
  let reads = 0;
  const gate = reloadGate({ reload: async () => { reads++; }, intervalMs: 2_000, hidden: () => hidden });
  const pour = (count) => {
    for (let i = 0; i < count; i++) {
      for (const event of [{ type: "alert_counts", counts }, upsert(alert(`x${i}`, { device_id: "d2" }))]) {
        if (alertEventEffect(event, deviceFilters).read) gate.request();
      }
      t.mock.timers.tick(2);
    }
  };

  pour(1_000);
  await flush();
  assert.equal(reads, 1, "two seconds of counts, one read");

  hidden = true;
  pour(5_000);
  await flush();
  assert.equal(reads, 1, "a hidden tab reads nothing");
  hidden = false;
  gate.shown();
  t.mock.timers.tick(2_000);
  await flush();
  assert.equal(reads, 2, "and one read catches up once it's shown");
  gate.stop();
});
