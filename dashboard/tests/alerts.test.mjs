import assert from "node:assert/strict";
import test from "node:test";
import {
  DEFAULT_ALERT_FILTERS,
  alertFilterParams,
  applyAlertChange,
  channelForm,
  channelFormProblem,
  channelInput,
  matchesAlertFilters,
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

  const list = [alert("a"), alert("b", { raised_at: 50 })];
  const resolved = applyAlertChange(list, alert("a", { state: "resolved" }), active);
  assert.deepEqual(resolved.map(({ id }) => id), ["b"], "a resolved alert leaves the active list");
  const raised = applyAlertChange(list, alert("c", { raised_at: 200 }), active);
  assert.deepEqual(raised.map(({ id }) => id), ["c", "a", "b"]);
  const all = applyAlertChange(list, alert("a", { state: "resolved" }), { ...active, state: "all" });
  assert.deepEqual(all.map(({ id }) => id), ["b", "a"]);
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
