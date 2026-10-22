// Alerts, the rules that raise them and the channels they notify, as the
// server sends them, and how the website words them. Imports carry the .ts
// extension so node:test can load this module directly.
import type { Tag } from "../agents/types.ts";
import { type TagMatch, describeTagTarget } from "../tags/model.ts";
import { CATEGORIES, CATEGORY_LABELS, type UpdateCategory } from "../updates/model.ts";

export type Severity = "info" | "warning" | "critical";
export type Condition =
  | "device_offline"
  | "disk_space"
  | "cpu"
  | "memory"
  | "script_failed"
  | "updates_pending"
  | "update_install_failed"
  | "reboot_pending";

// Which pending updates an updates_pending rule counts.
export type UpdateCategoryChoice = "any" | UpdateCategory;
export type AlertState = "open" | "acknowledged" | "resolved";
export type Resolution = "cleared" | "manual" | "not_applicable";
export type ThresholdUnit = "percent" | "gb";

// GET /v1/alerts
export type Alert = {
  id: string;
  rule_id: string;
  rule_name: string;
  severity: Severity;
  condition: Condition;
  device_id: string;
  device_name: string;
  company_id: string | null;
  company_name: string | null;
  // The volume of a disk space alert; empty otherwise.
  subject: string;
  state: AlertState;
  value: number | null;
  detail: string;
  script_run_id: string | null;
  // The failed install of an update_install_failed alert.
  update_run_id?: string | null;
  occurrences: number;
  raised_at: number;
  last_occurred_at: number;
  acknowledged_at: number | null;
  acknowledged_by: string | null;
  resolved_at: number | null;
  // Who resolved it; null when the server did.
  resolved_by: string | null;
  resolution: Resolution | null;
};

// How many alerts are unresolved; open ones by severity too.
export type AlertCounts = {
  open: number;
  acknowledged: number;
  open_info: number;
  open_warning: number;
  open_critical: number;
};

export type AlertList = { alerts: Alert[]; counts: AlertCounts; truncated: boolean };

// What the event socket sends about alerts.
export type AlertEvent =
  | { type: "alert_upsert"; alert: Alert; counts: AlertCounts }
  | { type: "alert_counts"; counts: AlertCounts };

// GET /v1/alert-rules
export type AlertRule = {
  id: string;
  name: string;
  severity: Severity;
  enabled: boolean;
  condition: Condition;
  threshold: number | null;
  threshold_unit: ThresholdUnit | null;
  duration_minutes: number | null;
  nonzero_exit_fails: boolean;
  // An updates_pending rule's category, and the days updates or a restart
  // may be pending.
  update_category?: UpdateCategoryChoice | null;
  days?: number | null;
  // Every device, chosen companies, or the devices with any or all of
  // chosen tags.
  target: RuleTarget;
  company_ids: string[];
  tag_ids: string[];
  tag_match: TagMatch;
  channel_ids: string[];
  notify_on_resolve: boolean;
  // The condition in words.
  description: string;
  created_at: number;
  updated_at: number;
};

export type ChannelKind = "email" | "webhook";

// GET /v1/notification-channels
export type Channel = {
  id: string;
  name: string;
  kind: ChannelKind;
  enabled: boolean;
  webhook_url: string | null;
  has_secret: boolean;
  user_ids: string[];
  role_ids: string[];
  last_delivery_at: number | null;
  last_delivery_error: string | null;
  created_at: number;
  updated_at: number;
};

export type ChannelList = { channels: Channel[]; email_available: boolean };

export type Recipients = {
  users: { id: string; display_name: string; email: string }[];
  roles: { id: string; name: string }[];
};

export const SEVERITIES: readonly Severity[] = ["critical", "warning", "info"];

export const SEVERITY_LABELS: Record<Severity, string> = {
  info: "Info",
  warning: "Warning",
  critical: "Critical",
};

export const CONDITION_LABELS: Record<Condition, string> = {
  device_offline: "Device offline",
  disk_space: "Low disk space",
  cpu: "High CPU",
  memory: "High memory",
  script_failed: "Script failed",
  updates_pending: "Updates pending",
  update_install_failed: "Update install failed",
  reboot_pending: "Restart pending",
};

export const UPDATE_CATEGORY_CHOICES: readonly UpdateCategoryChoice[] = ["any", ...CATEGORIES];

export const updateCategoryChoiceLabel = (choice: UpdateCategoryChoice) =>
  choice === "any" ? "Any update" : `${CATEGORY_LABELS[choice]} updates`;

export const MAX_DAYS = 365;

export const CONDITIONS = Object.keys(CONDITION_LABELS) as Condition[];

export const STATE_LABELS: Record<AlertState, string> = {
  open: "Open",
  acknowledged: "Acknowledged",
  resolved: "Resolved",
};

export const EMPTY_COUNTS: AlertCounts = { open: 0, acknowledged: 0, open_info: 0, open_warning: 0, open_critical: 0 };

const isObject = (value: unknown): value is Record<string, unknown> => typeof value === "object" && value !== null;

function isCounts(value: unknown): value is AlertCounts {
  return isObject(value) && ["open", "acknowledged", "open_info", "open_warning", "open_critical"].every((key) => typeof value[key] === "number");
}

function isAlert(value: unknown): value is Alert {
  return isObject(value)
    && typeof value.id === "string"
    && typeof value.rule_id === "string"
    && typeof value.device_id === "string"
    && typeof value.state === "string"
    && typeof value.raised_at === "number";
}

// An alert event from the socket, or null for anything else.
export function parseAlertEvent(event: unknown): AlertEvent | null {
  if (!isObject(event)) return null;
  if (event.type === "alert_counts" && isCounts(event.counts)) return { type: "alert_counts", counts: event.counts };
  if (event.type === "alert_upsert" && isCounts(event.counts) && isAlert(event.alert)) {
    return { type: "alert_upsert", alert: event.alert, counts: event.counts };
  }
  return null;
}

// The most severe open alert's severity, for the navigation's count.
export function topSeverity(counts: AlertCounts): Severity | null {
  if (counts.open_critical) return "critical";
  if (counts.open_warning) return "warning";
  if (counts.open_info) return "info";
  return null;
}

// Which alerts the list shows: "active" is open and acknowledged.
export type StateFilter = "active" | AlertState | "all";

export type AlertFilters = {
  state: StateFilter;
  severity: Severity | "";
  // A company ID, NO_COMPANY_FILTER, or "" for every company.
  company: string;
  device: string;
};

export const NO_COMPANY_FILTER = "none";
export const DEFAULT_ALERT_FILTERS: AlertFilters = { state: "active", severity: "", company: "", device: "" };

const STATE_FILTERS: readonly StateFilter[] = ["active", "open", "acknowledged", "resolved", "all"];

export function parseAlertFilters(search: URLSearchParams): AlertFilters {
  const state = search.get("state");
  const severity = search.get("severity");
  return {
    state: STATE_FILTERS.includes(state as StateFilter) ? (state as StateFilter) : "active",
    severity: SEVERITIES.includes(severity as Severity) ? (severity as Severity) : "",
    company: search.get("company") ?? "",
    device: search.get("device") ?? "",
  };
}

// The filters as the page's search string and the API's query: only those
// that differ from the defaults.
export function alertFilterParams(filters: AlertFilters) {
  const params = new URLSearchParams();
  if (filters.state !== "active") params.set("state", filters.state);
  if (filters.severity) params.set("severity", filters.severity);
  if (filters.company) params.set("company", filters.company);
  if (filters.device) params.set("device", filters.device);
  return params;
}

// Whether an alert belongs in a list with these filters.
export function matchesAlertFilters(alert: Alert, filters: AlertFilters) {
  const state = filters.state === "all"
    || (filters.state === "active" ? alert.state !== "resolved" : alert.state === filters.state);
  const company = !filters.company
    || (filters.company === NO_COMPANY_FILTER ? alert.company_id === null : alert.company_id === filters.company);
  return state
    && (!filters.severity || alert.severity === filters.severity)
    && company
    && (!filters.device || alert.device_id === filters.device);
}

const STATE_ORDER: Record<AlertState, number> = { open: 0, acknowledged: 1, resolved: 2 };

// Open first, then acknowledged, then resolved; newest first within each,
// as the server lists them.
export function sortAlerts(alerts: readonly Alert[]) {
  return [...alerts].sort((left, right) =>
    STATE_ORDER[left.state] - STATE_ORDER[right.state]
    || right.raised_at - left.raised_at
    || (right.id < left.id ? -1 : right.id > left.id ? 1 : 0));
}

// Applies a changed alert to a list with these filters.
export function applyAlertChange(alerts: readonly Alert[], changed: Alert, filters: AlertFilters) {
  const others = alerts.filter((alert) => alert.id !== changed.id);
  return sortAlerts(matchesAlertFilters(changed, filters) ? [...others, changed] : others);
}

// "Resolved by ada@example.com", "Resolved: the condition passed".
export function resolutionLabel(alert: Pick<Alert, "resolution" | "resolved_by">) {
  switch (alert.resolution) {
    case "manual": return alert.resolved_by ? `Resolved by ${alert.resolved_by}` : "Resolved";
    case "cleared": return "Resolved itself: the condition passed";
    case "not_applicable": return "Resolved: the rule no longer covers it";
    default: return "Resolved";
  }
}

// Script failures stay until a person resolves them; the rest resolve
// themselves.
export const clearsItself = (condition: Condition) => condition !== "script_failed" && condition !== "update_install_failed";

// The rule editor's fields, as text while being edited.
export type RuleTarget = "all" | "companies" | "tags";

export type RuleForm = {
  name: string;
  severity: Severity;
  enabled: boolean;
  condition: Condition;
  threshold: string;
  thresholdUnit: ThresholdUnit;
  duration: string;
  nonzeroExitFails: boolean;
  updateCategory: UpdateCategoryChoice;
  days: string;
  target: RuleTarget;
  companyIds: string[];
  tagIds: string[];
  tagMatch: TagMatch;
  channelIds: string[];
  notifyOnResolve: boolean;
};

const DEFAULTS: Record<Condition, { threshold: string; duration: string; days: string }> = {
  device_offline: { threshold: "", duration: "15", days: "" },
  disk_space: { threshold: "10", duration: "", days: "" },
  cpu: { threshold: "90", duration: "10", days: "" },
  memory: { threshold: "90", duration: "10", days: "" },
  script_failed: { threshold: "", duration: "", days: "" },
  updates_pending: { threshold: "", duration: "", days: "7" },
  update_install_failed: { threshold: "", duration: "", days: "" },
  reboot_pending: { threshold: "", duration: "", days: "3" },
};

export function ruleForm(rule: AlertRule | null): RuleForm {
  if (!rule) {
    return {
      name: "",
      severity: "warning",
      enabled: true,
      condition: "device_offline",
      threshold: DEFAULTS.device_offline.threshold,
      thresholdUnit: "percent",
      duration: DEFAULTS.device_offline.duration,
      nonzeroExitFails: false,
      updateCategory: "security",
      days: DEFAULTS.device_offline.days,
      target: "all",
      companyIds: [],
      tagIds: [],
      tagMatch: "any",
      channelIds: [],
      notifyOnResolve: false,
    };
  }
  return {
    name: rule.name,
    severity: rule.severity,
    enabled: rule.enabled,
    condition: rule.condition,
    threshold: rule.threshold === null ? DEFAULTS[rule.condition].threshold : String(rule.threshold),
    thresholdUnit: rule.threshold_unit ?? "percent",
    duration: rule.duration_minutes === null ? DEFAULTS[rule.condition].duration : String(rule.duration_minutes),
    nonzeroExitFails: rule.nonzero_exit_fails,
    updateCategory: rule.update_category ?? "security",
    days: rule.days === null || rule.days === undefined ? DEFAULTS[rule.condition].days : String(rule.days),
    target: rule.target,
    companyIds: rule.company_ids,
    tagIds: rule.tag_ids ?? [],
    tagMatch: rule.tag_match ?? "any",
    channelIds: rule.channel_ids,
    notifyOnResolve: rule.notify_on_resolve,
  };
}

// A new rule's form after choosing another condition: its usual thresholds.
export function withCondition(form: RuleForm, condition: Condition): RuleForm {
  return { ...form, condition, threshold: DEFAULTS[condition].threshold, duration: DEFAULTS[condition].duration, days: DEFAULTS[condition].days, thresholdUnit: "percent" };
}

export const usesDays = (condition: Condition) => condition === "updates_pending" || condition === "reboot_pending";

export const usesThreshold = (condition: Condition) => condition === "disk_space" || condition === "cpu" || condition === "memory";
export const usesDuration = (condition: Condition) => condition === "device_offline" || condition === "cpu" || condition === "memory";
export const MAX_DURATION: Partial<Record<Condition, number>> = { device_offline: 10_080, cpu: 60, memory: 60 };

// What keeps the form from saving, or null.
export function ruleFormProblem(form: RuleForm): string | null {
  if (!form.name.trim()) return "Name the rule.";
  if (usesThreshold(form.condition)) {
    const threshold = Number(form.threshold);
    const gigabytes = form.condition === "disk_space" && form.thresholdUnit === "gb";
    if (!form.threshold.trim() || !Number.isFinite(threshold) || threshold <= 0 || (!gigabytes && threshold >= 100)) {
      return gigabytes ? "Enter the free space in GB, above 0." : "Enter a percentage above 0 and below 100.";
    }
  }
  if (usesDuration(form.condition)) {
    const duration = Number(form.duration);
    const max = MAX_DURATION[form.condition] ?? 60;
    if (!Number.isInteger(duration) || duration < 1 || duration > max) return `Enter 1 to ${max.toLocaleString()} minutes.`;
  }
  if (usesDays(form.condition)) {
    const days = Number(form.days);
    if (!form.days.trim() || !Number.isInteger(days) || days < 1 || days > MAX_DAYS) return `Enter 1 to ${MAX_DAYS} days.`;
  }
  if (form.target === "companies" && !form.companyIds.length) return "Choose at least one company, or every device.";
  if (form.target === "tags" && !form.tagIds.length) return "Choose at least one tag, or every device.";
  return null;
}

// The form as the server takes it.
export function ruleInput(form: RuleForm) {
  return {
    name: form.name.trim(),
    severity: form.severity,
    enabled: form.enabled,
    condition: form.condition,
    threshold: usesThreshold(form.condition) ? Number(form.threshold) : null,
    threshold_unit: form.condition === "disk_space" ? form.thresholdUnit : null,
    duration_minutes: usesDuration(form.condition) ? Number(form.duration) : null,
    nonzero_exit_fails: form.condition === "script_failed" && form.nonzeroExitFails,
    update_category: form.condition === "updates_pending" ? form.updateCategory : null,
    days: usesDays(form.condition) ? Number(form.days) : null,
    target: form.target,
    company_ids: form.target === "companies" ? form.companyIds : [],
    tag_ids: form.target === "tags" ? form.tagIds : [],
    tag_match: form.tagMatch,
    channel_ids: form.channelIds,
    notify_on_resolve: form.notifyOnResolve,
  };
}

// Which devices a rule covers, in words.
export function scopeLabel(
  rule: Pick<AlertRule, "target" | "company_ids"> & Partial<Pick<AlertRule, "tag_ids" | "tag_match">>,
  companyNames: ReadonlyMap<string, string>,
  tags: readonly Tag[] = [],
) {
  if (rule.target === "all") return "Every device";
  if (rule.target === "tags") return describeTagTarget(rule.tag_ids ?? [], rule.tag_match ?? "any", tags);
  const names = rule.company_ids.map((id) => companyNames.get(id)).filter((name): name is string => Boolean(name));
  if (!names.length) return "No companies";
  return names.length <= 2 ? names.join(", ") : `${names.slice(0, 2).join(", ")} and ${names.length - 2} more`;
}

// The channel editor's fields. A saved webhook secret is never shown: it's
// kept, replaced or removed.
export type ChannelForm = {
  name: string;
  kind: ChannelKind;
  enabled: boolean;
  url: string;
  secret: string;
  secretMode: "keep" | "replace" | "remove";
  userIds: string[];
  roleIds: string[];
};

export function channelForm(channel: Channel | null, emailAvailable: boolean): ChannelForm {
  return {
    name: channel?.name ?? "",
    kind: channel?.kind ?? (emailAvailable ? "email" : "webhook"),
    enabled: channel?.enabled ?? true,
    url: channel?.webhook_url ?? "",
    secret: "",
    secretMode: channel?.has_secret ? "keep" : "replace",
    userIds: channel?.user_ids ?? [],
    roleIds: channel?.role_ids ?? [],
  };
}

export function channelFormProblem(form: ChannelForm): string | null {
  if (!form.name.trim()) return "Name the channel.";
  if (form.kind === "email") return form.userIds.length || form.roleIds.length ? null : "Choose who to email.";
  try {
    const url = new URL(form.url.trim());
    if (url.protocol !== "https:" && url.protocol !== "http:") return "The URL must start with https:// or http://.";
  } catch {
    return "Enter the webhook's URL.";
  }
  return null;
}

// The form as the server takes it. A missing webhook_secret keeps the saved
// one; null removes it.
export function channelInput(form: ChannelForm) {
  const base = { name: form.name.trim(), kind: form.kind, enabled: form.enabled };
  if (form.kind === "email") return { ...base, user_ids: form.userIds, role_ids: form.roleIds };
  const input: Record<string, unknown> = { ...base, webhook_url: form.url.trim() };
  if (form.secretMode === "remove") input.webhook_secret = null;
  else if (form.secretMode === "replace") input.webhook_secret = form.secret.trim() ? form.secret : null;
  return input;
}
