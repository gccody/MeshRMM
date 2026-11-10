// Jobs and automation policies as the server sends them, and how the website
// words them. See docs/automation.md. Imports carry the .ts extension so
// node:test can load this module directly.
import type { Agent, DevicePlatform, Tag } from "../agents/types.ts";
import { type TagMatch, describeTagTarget, hasTags } from "../tags/model.ts";
import { type RunAs, type ScriptLanguage, LANGUAGE_LABELS, runsOn } from "../toolbox/model.ts";
import type { Permission } from "../auth/types.ts";
import { DEFAULT_REBOOT_DELAY_MINUTES, type RebootBehavior, type UpdateCategory, installName, rebootDelayProblem } from "../updates/model.ts";
import { MAX_RUN_ITEMS, type UpdateMode } from "../applications/model.ts";

export type JobKind = "script" | "file" | "update_scan" | "update_install" | "wake" | "app_install" | "app_update" | "app_uninstall";
export const isUpdateJob = (kind: JobKind) => kind === "update_scan" || kind === "update_install";
export const isApplicationJob = (kind: JobKind) => kind === "app_install" || kind === "app_update" || kind === "app_uninstall";

export const JOB_KIND_LABELS: Record<JobKind, string> = {
  script: "Script",
  file: "File",
  update_scan: "Update scan",
  update_install: "Update install",
  wake: "Wake",
  app_install: "Application install",
  app_update: "Application update",
  app_uninstall: "Application uninstall",
};
export type TargetKind = "devices" | "companies" | "tags" | "all";
export type OfflineChoice = "skip" | "wait";
export type Destination = "user" | "public";

// How a job went on one device.
export type Outcome =
  | "queued"
  | "waiting"
  | "running"
  | "succeeded"
  | "failed"
  | "timed_out"
  | "no_result"
  | "skipped"
  | "missed"
  | "cancelled";

export type JobCounts = { total: number } & Record<Outcome, number>;

// GET /v1/jobs, and the event socket's job_update.
export type Job = {
  id: string;
  kind: JobKind;
  item_id: string;
  item_name: string;
  language?: ScriptLanguage;
  run_as?: RunAs;
  destination?: Destination;
  // An update install's categories (empty for every update) and restart.
  categories?: UpdateCategory[];
  reboot?: RebootBehavior;
  reboot_delay_minutes?: number;
  target: TargetKind;
  // The device's name, the companies' or tags' names, or how many devices.
  target_label: string;
  offline: OfflineChoice;
  // When offline devices stop being waited for.
  expires_at_unix_ms: number;
  // The user who started it, or whose access the policy ran with.
  requested_by: string;
  policy_id: string | null;
  policy_name: string | null;
  created_at_unix_ms: number;
  finished_at_unix_ms: number | null;
  counts: JobCounts;
  requested_by_you: boolean;
  can_cancel: boolean;
};

// A device's part in a job.
export type JobDevice = {
  device_id: string;
  // Null once the device is gone for good.
  device_name: string | null;
  platform: DevicePlatform | null;
  outcome: Outcome;
  // Why it was skipped or missed, or its run's or delivery's error.
  detail: string | null;
  run_id: string | null;
  delivery_id: string | null;
  // The device's update scan or install, for update jobs.
  update_run_id?: string | null;
  // The device's wake request, for wake jobs.
  wake_id?: string | null;
  // The device's application run, for application jobs.
  application_run_id?: string | null;
  // The applications that failed or were skipped on it, in order.
  applications?: ApplicationProblem[];
  exit_code: number | null;
  // Where a delivered file was saved.
  path: string | null;
  started_at_unix_ms: number | null;
  completed_at_unix_ms: number | null;
};

// An application that failed or was skipped on a device, and why.
export type ApplicationProblem = { name: string; outcome: "failed" | "skipped"; error: string | null };

// "Firefox failed: winget exited with 1", one line per application.
export const applicationProblems = (device: Pick<JobDevice, "applications">) =>
  (device.applications ?? []).map((problem) => `${problem.name} ${problem.outcome}${problem.error ? `: ${problem.error}` : ""}`);

// GET /v1/jobs/{id}/devices: a page of the job's devices, in device ID
// order, and how many have the outcome asked for (or how many there are).
export type JobDevicesPage = { devices: JobDevice[]; matching: number };

export const JOB_DEVICES_PAGE = 100;
// The most the server sends in one page.
export const MAX_JOB_DEVICES_PAGE = 200;

// GET /v1/jobs/{id}/devices's address for the devices with `outcome` (or
// every one) after the device `after` (or from the start).
export function jobDevicesPath(jobId: string, outcome: Outcome | null, after: string | null, limit = JOB_DEVICES_PAGE) {
  const params = new URLSearchParams();
  if (outcome) params.set("outcome", outcome);
  if (after) params.set("after", after);
  params.set("limit", String(limit));
  return `/v1/jobs/${encodeURIComponent(jobId)}/devices?${params}`;
}

// Which devices a list is for: an outcome's, or "all".
export const jobDevicesKey = (outcome: Outcome | null) => outcome ?? "all";

// The devices loaded for one outcome chip (`key`), in the server's order,
// with how many match. `ticket` numbers the request the list was last
// loaded or refreshed from the start by; `end` is whether the last page
// reached the end.
export type JobDeviceList = { key: string; devices: JobDevice[]; matching: number; ticket: number; end: boolean };

// The server's answer to request `ticket` for the chip `key`: the devices
// from the start (a first load, or a refresh of the rows loaded), or the
// next page after the device `after`. `end` is whether it reached the end.
export type JobDevicesAnswer =
  | { kind: "start"; key: string; ticket: number; devices: JobDevice[]; matching: number; end: boolean }
  | { kind: "more"; key: string; ticket: number; after: string; devices: JobDevice[]; matching: number; end: boolean };

const ids = (devices: readonly JobDevice[]) => new Set(devices.map((device) => device.device_id));

// The list after an answer, while the page wants the chip `wanted`. An
// answer for another chip is dropped, and so is a load from the start older
// than the one the list shows. A refresh puts its rows in place of those it
// covers and keeps the rows after them, so loaded rows keep their places
// and none shows twice. A next page joins only after the row it was asked
// after, if that is still the last: devices move between outcomes, and the
// list can't tell where else it would go.
export function applyJobDevicesAnswer(list: JobDeviceList | null, answer: JobDevicesAnswer, wanted: string): JobDeviceList | null {
  if (answer.key !== wanted) return list;
  if (answer.kind === "start") {
    if (list && list.key === answer.key && answer.ticket < list.ticket) return list;
    const fresh = { key: answer.key, devices: answer.devices, matching: answer.matching, ticket: answer.ticket, end: answer.end };
    const last = answer.devices.at(-1);
    if (!list || list.key !== answer.key || answer.end || !last) return fresh;
    const place = list.devices.findIndex((device) => device.device_id === last.device_id);
    if (place === -1) return fresh;
    const shown = ids(answer.devices);
    const rest = list.devices.slice(place + 1).filter((device) => !shown.has(device.device_id));
    return rest.length ? { ...fresh, devices: [...answer.devices, ...rest], end: list.end } : fresh;
  }
  if (!list || list.key !== answer.key || list.devices.at(-1)?.device_id !== answer.after) return list;
  const loaded = ids(list.devices);
  return {
    ...list,
    devices: [...list.devices, ...answer.devices.filter((device) => !loaded.has(device.device_id))],
    matching: answer.ticket > list.ticket ? answer.matching : list.matching,
    end: answer.end,
  };
}

// How many more devices "Show more" can load.
export const moreJobDevices = (list: JobDeviceList | null) =>
  list && !list.end ? Math.max(0, list.matching - list.devices.length) : 0;

// When the loaded rows are next read, at most once every `interval` since
// they last were.
export const devicesRefreshDelay = (lastAt: number, now: number, interval: number) => Math.max(0, lastAt + interval - now);

// Whether a job's update changes what its page shows.
export const jobChanged = (current: Job, next: Job) =>
  current.finished_at_unix_ms !== next.finished_at_unix_ms || OUTCOMES.some((outcome) => current.counts[outcome] !== next.counts[outcome]) || current.counts.total !== next.counts.total;

// What the event socket sends about jobs.
export type JobEvent = { type: "job_update"; job: Job } | { type: "jobs_resync" };

export type TriggerKind = "interval" | "daily" | "weekly" | "once" | "enroll";

// What a policy does each time it runs.
export type PolicyAction = "script" | "update_scan" | "update_install" | "wake" | "app_install" | "app_update";

export const ACTION_LABELS: Record<PolicyAction, string> = {
  script: "Run a script",
  update_scan: "Scan for updates",
  update_install: "Install updates",
  wake: "Wake devices",
  app_install: "Install applications",
  app_update: "Update applications",
};

// GET /v1/automation-policies
export type Policy = {
  id: string;
  name: string;
  enabled: boolean;
  action: PolicyAction;
  // A script policy's script and account; null for update policies.
  script_id: string | null;
  // Null once the script is gone.
  script_name: string | null;
  script_language: ScriptLanguage | null;
  script_shared: boolean;
  run_as: RunAs | null;
  // An install policy's categories (empty for every update) and restart.
  update_categories?: UpdateCategory[] | null;
  reboot?: RebootBehavior | null;
  reboot_delay_minutes?: number | null;
  // An application policy's library applications and packages (as
  // source:id), and for an update which outdated ones it covers.
  applications?: ApplicationPolicySpec | null;
  target: TargetKind;
  device_ids: string[];
  company_ids: string[];
  tag_ids: string[];
  tag_match: TagMatch;
  trigger: TriggerKind;
  interval_minutes?: number;
  // HH:MM, for daily and weekly policies.
  time?: string;
  // ISO days, Monday = 1.
  weekdays?: number[];
  // YYYY-MM-DDTHH:MM, for a policy that runs once.
  run_at?: string;
  time_zone?: string;
  offline: OfflineChoice;
  catch_up_hours: number;
  channel_ids: string[];
  // The user whose access it runs with.
  owner: string;
  owned_by_you: boolean;
  next_run_at: number | null;
  last_run_at: number | null;
  last_job_id: string | null;
  // Why it last didn't run when it was due.
  last_problem: string | null;
  last_problem_at: number | null;
  created_at: number;
  updated_at: number;
  // The list's latest job of the policy.
  last_job?: Job | null;
};

export const MIN_INTERVAL_MINUTES = 5;
export const MAX_INTERVAL_MINUTES = 28 * 24 * 60;
export const MAX_WAIT_HOURS = 7 * 24;
export const DEFAULT_WAIT_HOURS = 24;
export const MAX_POLICY_NAME_CHARS = 120;

export const OUTCOMES: readonly Outcome[] = [
  "succeeded",
  "failed",
  "timed_out",
  "no_result",
  "skipped",
  "missed",
  "cancelled",
  "running",
  "queued",
  "waiting",
];

export const OUTCOME_LABELS: Record<Outcome, string> = {
  queued: "Queued",
  waiting: "Waiting for the device",
  running: "Running",
  succeeded: "Succeeded",
  failed: "Failed",
  timed_out: "Timed out",
  no_result: "No result",
  skipped: "Skipped",
  missed: "Missed",
  cancelled: "Cancelled",
};

// A wake job's device is "running" while the job waits for it to come
// online, which is what succeeding means.
const WAKE_OUTCOME_LABELS: Partial<Record<Outcome, string>> = {
  queued: "Waking",
  running: "Waiting to come online",
  succeeded: "Came online",
  no_result: "Didn’t come online",
  skipped: "Online already",
};

// The outcome's label in a job of `kind`.
export const outcomeLabel = (outcome: Outcome, kind?: JobKind) =>
  (kind === "wake" ? WAKE_OUTCOME_LABELS[outcome] : undefined) ?? OUTCOME_LABELS[outcome];

// Short words for counts: "3 failed".
const COUNT_WORDS: Record<Outcome, string> = {
  queued: "queued",
  waiting: "waiting",
  running: "running",
  succeeded: "succeeded",
  failed: "failed",
  timed_out: "timed out",
  no_result: "no result",
  skipped: "skipped",
  missed: "missed",
  cancelled: "cancelled",
};

export type OutcomeTone = "pending" | "success" | "problem" | "muted";

export function outcomeTone(outcome: Outcome): OutcomeTone {
  switch (outcome) {
    case "succeeded": return "success";
    case "failed":
    case "timed_out":
    case "no_result": return "problem";
    case "queued":
    case "waiting":
    case "running": return "pending";
    default: return "muted";
  }
}

const ACTIVE: readonly Outcome[] = ["queued", "waiting", "running"];

export const activeCount = (counts: JobCounts) => ACTIVE.reduce((sum, outcome) => sum + counts[outcome], 0);
export const failureCount = (counts: JobCounts) => counts.failed + counts.timed_out + counts.no_result;

// Whether the job may still do something.
export const isJobFinished = (job: Pick<Job, "finished_at_unix_ms" | "counts">) =>
  job.finished_at_unix_ms !== null || activeCount(job.counts) === 0;

// "12 succeeded · 1 failed · 3 skipped": the non-zero outcomes, results first.
export function countsSummary(counts: JobCounts) {
  const parts = OUTCOMES.filter((outcome) => counts[outcome] > 0).map((outcome) => `${counts[outcome]} ${COUNT_WORDS[outcome]}`);
  return parts.length ? parts.join(" · ") : "No devices";
}

// The bar's segments, as fractions of the total from `start`, in the order
// they show.
export function progressSegments(counts: JobCounts) {
  const total = Math.max(counts.total, 1);
  const segments: { tone: OutcomeTone; value: number; label: string }[] = [
    { tone: "success", value: counts.succeeded, label: "succeeded" },
    { tone: "problem", value: failureCount(counts), label: "failed, timed out or no result" },
    { tone: "muted", value: counts.skipped + counts.missed + counts.cancelled, label: "skipped, missed or cancelled" },
    { tone: "pending", value: counts.running, label: "running" },
  ];
  let start = 0;
  return segments.filter((segment) => segment.value > 0).map((segment) => {
    const fraction = segment.value / total;
    const placed = { ...segment, start, fraction };
    start += fraction;
    return placed;
  });
}

// How many devices are done, as "7 of 10 done".
export function progressLabel(counts: JobCounts) {
  return `${counts.total - activeCount(counts)} of ${counts.total} done`;
}

// Who started a job: the policy, the user, or a teammate.
export function jobStarter(job: Pick<Job, "policy_name" | "requested_by_you" | "requested_by">) {
  if (job.policy_name !== null) return `Policy “${job.policy_name}”`;
  return job.requested_by_you ? "You" : job.requested_by;
}

// A device's result in a few words: the exit code, or why.
export function deviceResult(device: Pick<JobDevice, "outcome" | "exit_code" | "detail" | "path">) {
  if (device.outcome === "failed" && device.exit_code !== null && device.exit_code !== 0) return `Exit code ${device.exit_code}`;
  if (device.outcome === "succeeded" && device.path) return device.path;
  return device.detail ?? "";
}

// The job event in a socket message, or null for another kind of message.
export function parseJobEvent(event: unknown): JobEvent | null {
  if (!event || typeof event !== "object") return null;
  const value = event as Record<string, unknown>;
  if (value.type === "jobs_resync") return { type: "jobs_resync" };
  if (value.type === "job_update" && isJob(value.job)) return { type: "job_update", job: value.job };
  return null;
}

function isJob(value: unknown): value is Job {
  if (!value || typeof value !== "object") return false;
  const job = value as Record<string, unknown>;
  return typeof job.id === "string" && typeof job.item_name === "string" && !!job.counts && typeof job.counts === "object";
}

// A job from the socket in place of the list's copy, newest first. With
// `policyId`, only that policy's jobs belong in the list.
export function applyJobUpdate(jobs: readonly Job[], job: Job, policyId: string | null = null) {
  if (policyId !== null && job.policy_id !== policyId) return [...jobs];
  const others = jobs.filter((candidate) => candidate.id !== job.id);
  return [job, ...others].sort((left, right) => right.created_at_unix_ms - left.created_at_unix_ms || right.id.localeCompare(left.id));
}

// ── Starting a job ─────────────────────────────────────────

export type JobTarget = {
  kind: TargetKind;
  deviceIds: readonly string[];
  companyIds: readonly string[];
  tagIds?: readonly string[];
  tagMatch?: TagMatch;
};

// The devices a target covers, from the browser's inventory.
export function coveredAgents(agents: readonly Agent[], target: JobTarget) {
  if (target.kind === "all") return [...agents];
  if (target.kind === "companies") {
    const companies = new Set(target.companyIds);
    return agents.filter((agent) => agent.company_id && companies.has(agent.company_id));
  }
  if (target.kind === "tags") {
    const tags = target.tagIds ?? [];
    return tags.length ? agents.filter((agent) => hasTags(agent, tags, target.tagMatch ?? "any")) : [];
  }
  const chosen = new Set(target.deviceIds);
  return agents.filter((agent) => chosen.has(agent.id));
}

// What a job will do on the devices it covers, before it starts: how many
// it covers, skip for want of an interpreter, and find offline.
export function jobPreview(agents: readonly Agent[], target: JobTarget, language: ScriptLanguage | null) {
  const covered = coveredAgents(agents, target);
  // One pass: the skipped aren't counted again as offline.
  let skipped = 0;
  let offline = 0;
  for (const agent of covered) {
    if (language && !runsOn(language, agent.platform)) skipped++;
    else if (!agent.connected) offline++;
  }
  return { devices: covered.length, skipped, offline };
}

// The preview in words.
export function previewText(preview: ReturnType<typeof jobPreview>, language: ScriptLanguage | null, offline: OfflineChoice, waitHours: number) {
  if (!preview.devices) return "No devices are chosen.";
  const parts = [`Runs on ${preview.devices} ${preview.devices === 1 ? "device" : "devices"}.`];
  if (preview.skipped && language) {
    parts.push(`${preview.skipped} will be skipped: they have no ${LANGUAGE_LABELS[language]} interpreter.`);
  }
  if (preview.offline) {
    const are = preview.offline === 1 ? "1 is" : `${preview.offline} are`;
    parts.push(offline === "wait"
      ? `${are} offline and will run if ${preview.offline === 1 ? "it connects" : "they connect"} within ${waitHours} ${waitHours === 1 ? "hour" : "hours"}.`
      : `${are} offline and will be skipped.`);
  }
  return parts.join(" ");
}

// Why the wait can't be used, or null.
export function waitHoursProblem(text: string) {
  const hours = Number(text);
  return Number.isInteger(hours) && hours >= 1 && hours <= MAX_WAIT_HOURS ? null : `Wait 1 to ${MAX_WAIT_HOURS} hours.`;
}

// ── Policies ───────────────────────────────────────────────

export const WEEKDAY_LABELS = ["Mon", "Tue", "Wed", "Thu", "Fri", "Sat", "Sun"] as const;

export const TRIGGER_LABELS: Record<TriggerKind, string> = {
  interval: "Every few minutes or hours",
  daily: "Daily",
  weekly: "Weekly",
  once: "Once",
  enroll: "When a device enrolls",
};

// When the policy runs, in words.
export function describeSchedule(policy: Pick<Policy, "trigger" | "interval_minutes" | "time" | "weekdays" | "run_at" | "time_zone">) {
  const zone = policy.time_zone ? ` (${policy.time_zone})` : "";
  switch (policy.trigger) {
    case "interval": {
      const minutes = policy.interval_minutes ?? 0;
      if (minutes % 60 === 0) {
        const hours = minutes / 60;
        return hours === 1 ? "Every hour" : `Every ${hours} hours`;
      }
      return `Every ${minutes} minutes`;
    }
    case "daily": return `Daily at ${policy.time}${zone}`;
    case "weekly": {
      const days = (policy.weekdays ?? []).map((day) => WEEKDAY_LABELS[day - 1]).join(", ");
      return `Weekly on ${days} at ${policy.time}${zone}`;
    }
    case "once": return `Once on ${(policy.run_at ?? "").replace("T", " at ")}${zone}`;
    case "enroll": return "When a device enrolls";
  }
}

export type ApplicationPolicySpec = { application_ids: string[]; packages: string[]; mode: UpdateMode };

// What an application update policy covers, in words.
function describeApplicationUpdate(spec: ApplicationPolicySpec | null | undefined) {
  const count = (spec?.application_ids.length ?? 0) + (spec?.packages.length ?? 0);
  const what = count === 1 ? "1 application" : `${count} applications`;
  if (!spec || spec.mode === "all") return "Update all applications";
  return spec.mode === "only" ? `Update ${what}` : `Update all applications except ${what}`;
}

// What a policy does, in words: its script, the updates it scans or
// installs, or the applications it installs or updates.
export function describePolicyAction(policy: Pick<Policy, "action" | "script_name" | "update_categories"> & Partial<Pick<Policy, "applications">>) {
  switch (policy.action) {
    case "app_install": {
      const count = policy.applications?.application_ids.length ?? 0;
      return count === 1 ? "Install 1 application" : `Install ${count} applications`;
    }
    case "app_update": return describeApplicationUpdate(policy.applications);
    case "update_scan": return "Scan for updates";
    case "update_install": return installName(policy.update_categories ?? []);
    case "wake": return "Wake devices";
    default: return policy.script_name ?? "Deleted script";
  }
}

// The permission saving a policy of `action` needs besides
// automation.manage: the policy runs with its saver's access.
export const SAVE_PERMISSION: Record<PolicyAction, Permission> = {
  script: "scripts.run",
  update_scan: "updates.view",
  update_install: "updates.install",
  wake: "devices.wake",
  app_install: "applications.deploy",
  app_update: "applications.deploy",
};

// Which devices a policy runs on, in words.
export function describeTarget(
  policy: Pick<Policy, "target" | "company_ids" | "device_ids"> & Partial<Pick<Policy, "tag_ids" | "tag_match">>,
  companyNames: ReadonlyMap<string, string>,
  deviceNames: { get(id: string): string | undefined },
  tags: readonly Tag[] = [],
) {
  if (policy.target === "all") return "All devices";
  if (policy.target === "tags") return describeTagTarget(policy.tag_ids ?? [], policy.tag_match ?? "any", tags);
  const ids = policy.target === "companies" ? policy.company_ids : policy.device_ids;
  const names = policy.target === "companies" ? companyNames : deviceNames;
  const what = policy.target === "companies" ? "companies" : "devices";
  if (ids.length > 3) return `${ids.length} ${what}`;
  return ids.map((id) => names.get(id) ?? (policy.target === "companies" ? "Deleted company" : "Deleted device")).join(", ") || `No ${what}`;
}

// What the policy editor holds. Numbers stay text while they are typed.
export type PolicyDraft = {
  name: string;
  enabled: boolean;
  action: PolicyAction;
  scriptId: string;
  runAs: RunAs;
  target: TargetKind;
  deviceIds: string[];
  companyIds: string[];
  tagIds: string[];
  tagMatch: TagMatch;
  trigger: TriggerKind;
  intervalValue: string;
  intervalUnit: "minutes" | "hours";
  time: string;
  weekdays: number[];
  runAt: string;
  timeZone: string;
  offline: OfflineChoice;
  catchUpHours: string;
  channelIds: string[];
  updateCategories: UpdateCategory[];
  reboot: RebootBehavior;
  rebootDelayMinutes: number;
  applicationIds: string[];
  // Packages as source:id, one a line.
  packages: string;
  updateMode: UpdateMode;
};

// The draft of a new policy, or of `policy` to edit.
export function policyDraft(policy: Policy | null, timeZone: string, action: PolicyAction = "script"): PolicyDraft {
  const minutes = policy?.interval_minutes ?? 60;
  const inHours = minutes % 60 === 0;
  return {
    name: policy?.name ?? "",
    enabled: policy?.enabled ?? true,
    action: policy?.action ?? action,
    scriptId: policy?.script_id ?? "",
    runAs: policy?.run_as ?? "system",
    target: policy?.target ?? "all",
    deviceIds: [...(policy?.device_ids ?? [])],
    companyIds: [...(policy?.company_ids ?? [])],
    tagIds: [...(policy?.tag_ids ?? [])],
    tagMatch: policy?.tag_match ?? "any",
    trigger: policy?.trigger ?? "daily",
    intervalValue: String(inHours ? minutes / 60 : minutes),
    intervalUnit: inHours ? "hours" : "minutes",
    time: policy?.time ?? "03:00",
    weekdays: [...(policy?.weekdays ?? [1, 2, 3, 4, 5])],
    runAt: policy?.run_at ?? "",
    timeZone: policy?.time_zone ?? timeZone,
    offline: policy?.offline ?? "wait",
    catchUpHours: String(policy?.catch_up_hours ?? DEFAULT_WAIT_HOURS),
    channelIds: [...(policy?.channel_ids ?? [])],
    updateCategories: [...(policy?.update_categories ?? (policy ? [] : ["security", "critical"]))],
    reboot: policy?.reboot ?? "if_required",
    rebootDelayMinutes: policy?.reboot_delay_minutes ?? DEFAULT_REBOOT_DELAY_MINUTES,
    applicationIds: [...(policy?.applications?.application_ids ?? [])],
    packages: (policy?.applications?.packages ?? []).join("\n"),
    updateMode: policy?.applications?.mode ?? "all",
  };
}

const TIME = /^([01]\d|2[0-3]):[0-5]\d$/u;
const DATE_TIME = /^(\d{4})-(\d{2})-(\d{2})T([01]\d|2[0-3]):[0-5]\d$/u;
// Unicode control characters, which names may not contain.
// eslint-disable-next-line no-control-regex
const CONTROL = /[\u0000-\u001f\u007f-\u009f]/u;

const usesZone = (trigger: TriggerKind) => trigger === "daily" || trigger === "weekly" || trigger === "once";

export function intervalMinutes(draft: Pick<PolicyDraft, "intervalValue" | "intervalUnit">) {
  const value = Number(draft.intervalValue);
  return draft.intervalUnit === "hours" ? value * 60 : value;
}

function validDate(text: string) {
  const match = DATE_TIME.exec(text);
  if (!match) return false;
  const [year, month, day] = [Number(match[1]), Number(match[2]), Number(match[3])];
  const date = new Date(Date.UTC(year, month - 1, day));
  return date.getUTCFullYear() === year && date.getUTCMonth() === month - 1 && date.getUTCDate() === day;
}

// Why the draft can't be saved, or null when it can. `zones` are the time
// zones the browser knows; an empty list accepts any.
export function policyDraftProblem(draft: PolicyDraft, zones: readonly string[] = []): string | null {
  const name = draft.name.trim();
  if (!name || Array.from(name).length > MAX_POLICY_NAME_CHARS || CONTROL.test(name)) {
    return `Give the policy a name of up to ${MAX_POLICY_NAME_CHARS} characters.`;
  }
  if (draft.action === "script" && !draft.scriptId) return "Choose a shared script to run.";
  if (draft.action === "app_install" && !draft.applicationIds.length) return "Choose the library applications to install.";
  if (draft.action === "app_update" && draft.updateMode !== "all") {
    const packages = packageLines(draft.packages);
    if (packages.some((line) => !PACKAGE_LINE.test(line))) return "Write each package as source:id, such as winget:Mozilla.Firefox.";
    if (!draft.applicationIds.length && !packages.length) return draft.updateMode === "only" ? "Choose the applications to update." : "Choose the applications to leave out.";
    if (draft.applicationIds.length + packages.length > MAX_RUN_ITEMS) return `Choose at most ${MAX_RUN_ITEMS} applications.`;
  }
  if (draft.action === "app_install" && draft.applicationIds.length > MAX_RUN_ITEMS) return `Choose at most ${MAX_RUN_ITEMS} applications.`;
  if (draft.action === "update_install" && draft.reboot !== "never") {
    const problem = rebootDelayProblem(draft.rebootDelayMinutes);
    if (problem) return problem;
  }
  if (draft.target === "companies" && !draft.companyIds.length) return "Choose at least one company.";
  if (draft.target === "devices" && !draft.deviceIds.length) return "Choose at least one device.";
  if (draft.target === "tags" && !draft.tagIds.length) return "Choose at least one tag.";
  if (draft.trigger === "enroll" && draft.action === "wake") return "A policy that wakes devices runs on a schedule: newly enrolled devices are online.";
  if (draft.trigger === "enroll" && (draft.target === "devices" || draft.target === "tags")) return "A policy for newly enrolled devices runs on every device or on companies.";
  if (draft.trigger === "interval") {
    const minutes = intervalMinutes(draft);
    if (!Number.isInteger(minutes) || minutes < MIN_INTERVAL_MINUTES || minutes > MAX_INTERVAL_MINUTES) {
      return `The interval must be ${MIN_INTERVAL_MINUTES} minutes to ${MAX_INTERVAL_MINUTES / (24 * 60)} days, in whole minutes.`;
    }
  }
  if ((draft.trigger === "daily" || draft.trigger === "weekly") && !TIME.test(draft.time)) return "Enter the time as HH:MM, from 00:00 to 23:59.";
  if (draft.trigger === "weekly" && !draft.weekdays.length) return "Choose the days of the week it runs on.";
  if (draft.trigger === "once" && !validDate(draft.runAt)) return "Choose the date and time it runs.";
  if (usesZone(draft.trigger) && (!draft.timeZone || (zones.length > 0 && !zones.includes(draft.timeZone)))) return "Choose a time zone.";
  if (draft.action !== "wake" && (draft.offline === "wait" || draft.trigger === "enroll")) {
    const problem = waitHoursProblem(draft.catchUpHours);
    if (problem) return `Catch offline devices up for 1 to ${MAX_WAIT_HOURS} hours.`;
  }
  return null;
}

// An update policy's packages, one `source:id` a line.
export const packageLines = (text: string) => text.split(/\r?\n/u).map((line) => line.trim()).filter(Boolean);
const PACKAGE_LINE = /^(winget|chocolatey|homebrew|homebrew_cask|flatpak|snap):[A-Za-z0-9][A-Za-z0-9._+@/-]*$/u;

// The request body that saves the draft. Call only when it has no problem.
export function policyInput(draft: PolicyDraft) {
  const body: Record<string, unknown> = {
    name: draft.name.trim(),
    enabled: draft.enabled,
    action: draft.action,
    target: draft.target,
    device_ids: draft.target === "devices" ? draft.deviceIds : [],
    company_ids: draft.target === "companies" ? draft.companyIds : [],
    tag_ids: draft.target === "tags" ? draft.tagIds : [],
    tag_match: draft.tagMatch,
    trigger: draft.trigger,
    // A wake policy is for offline devices; it never waits for them.
    offline: draft.action === "wake" ? "skip" : draft.offline,
    catch_up_hours: waitHoursProblem(draft.catchUpHours) ? DEFAULT_WAIT_HOURS : Number(draft.catchUpHours),
    channel_ids: draft.channelIds,
  };
  if (draft.action === "script") {
    body.script_id = draft.scriptId;
    body.run_as = draft.runAs;
  }
  if (draft.action === "update_install") {
    body.update_categories = [...draft.updateCategories];
    body.reboot = draft.reboot;
    body.reboot_delay_minutes = draft.rebootDelayMinutes;
  }
  if (draft.action === "app_install") body.applications = { application_ids: draft.applicationIds, packages: [], mode: "all" };
  if (draft.action === "app_update") {
    body.applications = draft.updateMode === "all"
      ? { application_ids: [], packages: [], mode: "all" }
      : { application_ids: draft.applicationIds, packages: packageLines(draft.packages), mode: draft.updateMode };
  }
  if (draft.trigger === "interval") body.interval_minutes = intervalMinutes(draft);
  if (draft.trigger === "daily" || draft.trigger === "weekly") body.time = draft.time;
  if (draft.trigger === "weekly") body.weekdays = [...draft.weekdays].sort((left, right) => left - right);
  if (draft.trigger === "once") body.run_at = draft.runAt;
  if (usesZone(draft.trigger)) body.time_zone = draft.timeZone;
  return body;
}

// Browsers list some zones by the names ICU kept from older tz releases;
// the tz database (and the server) know the current ones too.
const CURRENT_ZONE_NAMES: Record<string, string> = {
  "Africa/Asmera": "Africa/Asmara",
  "America/Buenos_Aires": "America/Argentina/Buenos_Aires",
  "America/Catamarca": "America/Argentina/Catamarca",
  "America/Coral_Harbour": "America/Atikokan",
  "America/Cordoba": "America/Argentina/Cordoba",
  "America/Godthab": "America/Nuuk",
  "America/Indianapolis": "America/Indiana/Indianapolis",
  "America/Jujuy": "America/Argentina/Jujuy",
  "America/Louisville": "America/Kentucky/Louisville",
  "America/Mendoza": "America/Argentina/Mendoza",
  "Asia/Calcutta": "Asia/Kolkata",
  "Asia/Katmandu": "Asia/Kathmandu",
  "Asia/Rangoon": "Asia/Yangon",
  "Asia/Saigon": "Asia/Ho_Chi_Minh",
  "Atlantic/Faeroe": "Atlantic/Faroe",
  "Europe/Kiev": "Europe/Kyiv",
  "Pacific/Enderbury": "Pacific/Kanton",
  "Pacific/Ponape": "Pacific/Pohnpei",
  "Pacific/Truk": "Pacific/Chuuk",
};

/** A zone by its current tz database name, such as Asia/Kolkata for Asia/Calcutta. */
export const currentZoneName = (zone: string) => CURRENT_ZONE_NAMES[zone] ?? zone;

// The time zones to offer, by their current names and in order, with UTC,
// which some browsers leave out.
export function timeZones(supported: readonly string[]) {
  const zones = new Set(supported.map(currentZoneName));
  zones.add("UTC");
  return [...zones].sort();
}
