// Reports and their schedules, as the server sends them, and the run and
// schedule forms. Imports carry the .ts extension so node:test can load this
// module directly.
import type { Company, Tag } from "../agents/types.ts";
import { type TagMatch, describeTagTarget } from "../tags/model.ts";

export type TemplateId = "executive_summary" | "patch_compliance" | "hardware_inventory" | "software_inventory" | "alerts";
export type ReportTarget = "all" | "companies" | "tags";
export type RollingPeriod = "last_7_days" | "last_30_days" | "previous_month";
export type PeriodKind = RollingPeriod | "custom";
export type Frequency = "daily" | "weekly" | "monthly";
export type ReportStatus = "queued" | "running" | "succeeded" | "failed";
export type Delivery = "pending" | "sent" | "failed" | "not_configured";

export type Template = {
  id: TemplateId;
  title: string;
  description: string;
  uses_period: boolean;
  uses_stale_days: boolean;
};

export type Templates = {
  templates: Template[];
  email_configured: boolean;
  retention_days: number;
  default_stale_days: number;
  max_stale_days: number;
  max_custom_days: number;
};

export type Report = {
  id: string;
  template: TemplateId;
  title: string | null;
  status: ReportStatus;
  error: string | null;
  trigger: "manual" | "schedule" | "run_now";
  schedule_id: string | null;
  schedule_name: string | null;
  requested_by: string;
  requested_by_you: boolean;
  scope_label: string;
  period: PeriodKind;
  period_start: number;
  period_end: number;
  period_label: string | null;
  time_zone: string;
  created_at: number;
  finished_at: number | null;
  expires_at: number;
  size_bytes: number | null;
  delivery: Delivery | null;
  delivery_detail: string | null;
  tables?: ReportTable[];
};

export type ReportTable = { id: string; section: string; title: string; rows: number };

export type ReportList = { reports: Report[]; next_before: number | null };

export type Schedule = {
  id: string;
  name: string;
  enabled: boolean;
  template: TemplateId;
  target: ReportTarget;
  company_ids: string[];
  tag_ids: string[];
  tag_match: TagMatch;
  period: RollingPeriod;
  stale_days: number;
  frequency: Frequency;
  time: string;
  weekday: number | null;
  day_of_month: number | null;
  time_zone: string;
  user_ids: string[];
  emails: string[];
  owner: string;
  owned_by_you: boolean;
  next_run_at: number | null;
  last_run_at: number | null;
  last_report_id: string | null;
};

export type Recipient = { id: string; display_name: string; email: string };

export const PERIOD_LABELS: Record<PeriodKind, string> = {
  last_7_days: "Last 7 days",
  last_30_days: "Last 30 days",
  previous_month: "Previous calendar month",
  custom: "Custom range",
};

export const FREQUENCY_LABELS: Record<Frequency, string> = { daily: "Daily", weekly: "Weekly", monthly: "Monthly" };

export const WEEKDAY_NAMES = ["Monday", "Tuesday", "Wednesday", "Thursday", "Friday", "Saturday", "Sunday"] as const;

export const STATUS_LABELS: Record<ReportStatus, string> = { queued: "Waiting", running: "Making", succeeded: "Ready", failed: "Failed" };

export const DELIVERY_LABELS: Record<Delivery, string> = {
  pending: "Sending",
  sent: "Emailed",
  failed: "Email failed",
  not_configured: "Not emailed",
};

export const MAX_RECIPIENTS = 50;
export const MAX_DAY_OF_MONTH = 28;

// Whether the list should be read again soon: a report is still being made,
// or its email is still being sent.
export const isWorking = (reports: readonly Report[]) =>
  reports.some((report) => report.status === "queued" || report.status === "running" || (report.status === "succeeded" && report.delivery === "pending"));

export const reportPage = (id: string) => `/v1/reports/${encodeURIComponent(id)}/html`;
export const reportDownload = (id: string) => `${reportPage(id)}?download`;
export const tableDownload = (id: string, table: string) => `/v1/reports/${encodeURIComponent(id)}/tables/${encodeURIComponent(table)}.csv`;

// Who or what made the report.
export function describeRequester(report: Pick<Report, "trigger" | "requested_by" | "requested_by_you" | "schedule_name">) {
  const who = report.requested_by_you ? "you" : report.requested_by;
  if (report.trigger === "schedule") return report.requested_by;
  if (report.trigger === "run_now") return `Run now by ${who}${report.schedule_name ? `, from “${report.schedule_name}”` : ""}`;
  return report.requested_by_you ? "You" : report.requested_by;
}

// When a schedule runs, in words.
export function describeFrequency(schedule: Pick<Schedule, "frequency" | "time" | "weekday" | "day_of_month" | "time_zone">) {
  const at = `at ${schedule.time} (${schedule.time_zone})`;
  if (schedule.frequency === "weekly" && schedule.weekday) return `Every ${WEEKDAY_NAMES[schedule.weekday - 1]} ${at}`;
  if (schedule.frequency === "monthly" && schedule.day_of_month) return `On day ${schedule.day_of_month} of each month ${at}`;
  return `Every day ${at}`;
}

// The devices a report or schedule covers, in words.
export function describeScope(target: ReportTarget, companyIds: readonly string[], tagIds: readonly string[], tagMatch: TagMatch, companies: readonly Company[], tags: readonly Tag[]) {
  if (target === "all") return "Every device";
  if (target === "tags") return describeTagTarget(tagIds, tagMatch, tags);
  const names = companyIds.map((id) => companies.find((company) => company.id === id)?.name ?? "a deleted company");
  return names.length ? names.join(", ") : "No companies";
}

export function formatSize(bytes: number | null) {
  if (bytes === null) return "";
  if (bytes < 1000) return `${bytes} B`;
  if (bytes < 1_000_000) return `${(bytes / 1000).toFixed(0)} kB`;
  return `${(bytes / 1_000_000).toFixed(1)} MB`;
}

// --- The run dialog -------------------------------------------------------

export type ScopeDraft = { target: ReportTarget; companyIds: string[]; tagIds: string[]; tagMatch: TagMatch };

export type RunDraft = ScopeDraft & {
  template: TemplateId;
  period: PeriodKind;
  from: string;
  to: string;
  timeZone: string;
  staleDays: string;
};

// A date as `YYYY-MM-DD` in the browser's zone.
const isoDate = (date: Date) => `${date.getFullYear()}-${String(date.getMonth() + 1).padStart(2, "0")}-${String(date.getDate()).padStart(2, "0")}`;

export function runDraft(template: TemplateId, timeZone: string, defaultStaleDays: number, today = new Date()): RunDraft {
  const weekAgo = new Date(today);
  weekAgo.setDate(today.getDate() - 7);
  return {
    template,
    target: "all",
    companyIds: [],
    tagIds: [],
    tagMatch: "any",
    period: "last_30_days",
    from: isoDate(weekAgo),
    to: isoDate(today),
    timeZone,
    staleDays: String(defaultStaleDays),
  };
}

function scopeProblem(draft: ScopeDraft) {
  if (draft.target === "companies" && draft.companyIds.length === 0) return "Choose at least one company.";
  if (draft.target === "tags" && draft.tagIds.length === 0) return "Choose at least one tag.";
  return null;
}

function staleProblem(staleDays: string, max: number) {
  const days = Number(staleDays);
  if (!/^\d+$/.test(staleDays.trim()) || days < 1 || days > max) return `Devices count as not seen after 1 to ${max} days.`;
  return null;
}

const DAY_MS = 86_400_000;

export function runDraftProblem(draft: RunDraft, templates: Pick<Templates, "templates" | "max_stale_days" | "max_custom_days">, zones: readonly string[], today = new Date()) {
  const template = templates.templates.find((candidate) => candidate.id === draft.template);
  if (!template) return "Choose a report.";
  const scope = scopeProblem(draft);
  if (scope) return scope;
  if (!zones.includes(draft.timeZone)) return "Choose a time zone.";
  if (template.uses_period && draft.period === "custom") {
    if (!draft.from || !draft.to) return "Choose the first and last day.";
    if (draft.to < draft.from) return "The last day comes before the first.";
    if (draft.to > isoDate(today)) return "The range can't end after today.";
    const days = Math.round((Date.parse(draft.to) - Date.parse(draft.from)) / DAY_MS) + 1;
    if (days > templates.max_custom_days) return `A range covers at most ${templates.max_custom_days} days.`;
  }
  if (template.uses_stale_days) return staleProblem(draft.staleDays, templates.max_stale_days);
  return null;
}

function scopeInput(draft: ScopeDraft) {
  return {
    target: draft.target,
    company_ids: draft.target === "companies" ? draft.companyIds : [],
    tag_ids: draft.target === "tags" ? draft.tagIds : [],
    tag_match: draft.tagMatch,
  };
}

// What POST /v1/reports takes.
export function runInput(draft: RunDraft) {
  return {
    template: draft.template,
    ...scopeInput(draft),
    period: draft.period,
    ...(draft.period === "custom" ? { from: draft.from, to: draft.to } : {}),
    time_zone: draft.timeZone,
    stale_days: Number(draft.staleDays),
  };
}

// --- The schedule editor --------------------------------------------------

export type ScheduleDraft = ScopeDraft & {
  name: string;
  enabled: boolean;
  template: TemplateId;
  period: RollingPeriod;
  staleDays: string;
  frequency: Frequency;
  time: string;
  weekday: number;
  dayOfMonth: string;
  timeZone: string;
  userIds: string[];
  emails: string;
};

export function scheduleDraft(schedule: Schedule | null, timeZone: string, defaultStaleDays: number): ScheduleDraft {
  if (!schedule) {
    return {
      name: "",
      enabled: true,
      template: "executive_summary",
      target: "all",
      companyIds: [],
      tagIds: [],
      tagMatch: "any",
      period: "last_7_days",
      staleDays: String(defaultStaleDays),
      frequency: "weekly",
      time: "07:00",
      weekday: 1,
      dayOfMonth: "1",
      timeZone,
      userIds: [],
      emails: "",
    };
  }
  return {
    name: schedule.name,
    enabled: schedule.enabled,
    template: schedule.template,
    target: schedule.target,
    companyIds: schedule.company_ids,
    tagIds: schedule.tag_ids,
    tagMatch: schedule.tag_match,
    period: schedule.period,
    staleDays: String(schedule.stale_days),
    frequency: schedule.frequency,
    time: schedule.time,
    weekday: schedule.weekday ?? 1,
    dayOfMonth: String(schedule.day_of_month ?? 1),
    timeZone: schedule.time_zone,
    userIds: schedule.user_ids,
    emails: schedule.emails.join("\n"),
  };
}

// The extra addresses, one per line or separated by commas.
export const parseEmails = (text: string) => [...new Set(text.split(/[\s,;]+/).map((email) => email.trim().toLowerCase()).filter(Boolean))];

const EMAIL = /^[^\s@]+@[^\s@]+\.[^\s@]+$/;

export function scheduleDraftProblem(draft: ScheduleDraft, zones: readonly string[], maxStaleDays: number) {
  if (!draft.name.trim()) return "Name the schedule.";
  const scope = scopeProblem(draft);
  if (scope) return scope;
  if (!/^\d{2}:\d{2}$/.test(draft.time)) return "Choose a time.";
  if (!zones.includes(draft.timeZone)) return "Choose a time zone.";
  if (draft.frequency === "monthly") {
    const day = Number(draft.dayOfMonth);
    if (!/^\d+$/.test(draft.dayOfMonth.trim()) || day < 1 || day > MAX_DAY_OF_MONTH) return `Choose a day of the month from 1 to ${MAX_DAY_OF_MONTH}.`;
  }
  const emails = parseEmails(draft.emails);
  const wrong = emails.find((email) => !EMAIL.test(email));
  if (wrong) return `“${wrong}” isn't an email address.`;
  if (emails.length + draft.userIds.length > MAX_RECIPIENTS) return `A schedule emails at most ${MAX_RECIPIENTS} recipients.`;
  return staleProblem(draft.staleDays, maxStaleDays);
}

// What POST and PUT /v1/report-schedules take.
export function scheduleInput(draft: ScheduleDraft) {
  return {
    name: draft.name.trim(),
    enabled: draft.enabled,
    template: draft.template,
    ...scopeInput(draft),
    period: draft.period,
    stale_days: Number(draft.staleDays),
    frequency: draft.frequency,
    time: draft.time,
    weekday: draft.frequency === "weekly" ? draft.weekday : null,
    day_of_month: draft.frequency === "monthly" ? Number(draft.dayOfMonth) : null,
    time_zone: draft.timeZone,
    user_ids: draft.userIds,
    emails: parseEmails(draft.emails),
  };
}
