// Jobs and automation policies as the server sends them, and how the website
// words them. See docs/automation.md. Imports carry the .ts extension so
// node:test can load this module directly.
import type { Agent, DevicePlatform } from "../agents/types.ts";
import { type RunAs, type ScriptLanguage, LANGUAGE_LABELS, runsOn } from "../toolbox/model.ts";
import type { Permission } from "../auth/types.ts";
import { DEFAULT_REBOOT_DELAY_MINUTES, type RebootBehavior, type UpdateCategory, installName, rebootDelayProblem } from "../updates/model.ts";

export type JobKind = "script" | "file" | "update_scan" | "update_install" | "wake";
export const isUpdateJob = (kind: JobKind) => kind === "update_scan" || kind === "update_install";

export const JOB_KIND_LABELS: Record<JobKind, string> = {
  script: "Script",
  file: "File",
  update_scan: "Update scan",
  update_install: "Update install",
  wake: "Wake",
};
export type TargetKind = "devices" | "companies" | "all";
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
  // The device's name, the companies' names, or how many devices.
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
  exit_code: number | null;
  // Where a delivered file was saved.
  path: string | null;
  started_at_unix_ms: number | null;
  completed_at_unix_ms: number | null;
};

// GET /v1/jobs/{id}
export type JobDetail = Job & { devices: JobDevice[] };

// What the event socket sends about jobs.
export type JobEvent = { type: "job_update"; job: Job } | { type: "jobs_resync" };

export type TriggerKind = "interval" | "daily" | "weekly" | "once" | "enroll";

// What a policy does each time it runs.
export type PolicyAction = "script" | "update_scan" | "update_install" | "wake";

export const ACTION_LABELS: Record<PolicyAction, string> = {
  script: "Run a script",
  update_scan: "Scan for updates",
  update_install: "Install updates",
  wake: "Wake devices",
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
  target: TargetKind;
  device_ids: string[];
  company_ids: string[];
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

export type JobTarget = { kind: TargetKind; deviceIds: readonly string[]; companyIds: readonly string[] };

// The devices a target covers, from the browser's inventory.
export function coveredAgents(agents: readonly Agent[], target: JobTarget) {
  if (target.kind === "all") return [...agents];
  if (target.kind === "companies") {
    const companies = new Set(target.companyIds);
    return agents.filter((agent) => agent.company_id && companies.has(agent.company_id));
  }
  const chosen = new Set(target.deviceIds);
  return agents.filter((agent) => chosen.has(agent.id));
}

// What a job will do on the devices it covers, before it starts: how many
// it covers, skip for want of an interpreter, and find offline.
export function jobPreview(agents: readonly Agent[], target: JobTarget, language: ScriptLanguage | null) {
  const covered = coveredAgents(agents, target);
  const unsupported = language ? covered.filter((agent) => !runsOn(language, agent.platform)) : [];
  const offline = covered.filter((agent) => !agent.connected && !unsupported.includes(agent));
  return { devices: covered.length, skipped: unsupported.length, offline: offline.length };
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

// What a policy does, in words: its script, or the updates it scans or installs.
export function describePolicyAction(policy: Pick<Policy, "action" | "script_name" | "update_categories">) {
  switch (policy.action) {
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
};

// Which devices a policy runs on, in words.
export function describeTarget(policy: Pick<Policy, "target" | "company_ids" | "device_ids">, companyNames: ReadonlyMap<string, string>, deviceNames: ReadonlyMap<string, string>) {
  if (policy.target === "all") return "All devices";
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
  if (draft.action === "update_install" && draft.reboot !== "never") {
    const problem = rebootDelayProblem(draft.rebootDelayMinutes);
    if (problem) return problem;
  }
  if (draft.target === "companies" && !draft.companyIds.length) return "Choose at least one company.";
  if (draft.target === "devices" && !draft.deviceIds.length) return "Choose at least one device.";
  if (draft.trigger === "enroll" && draft.action === "wake") return "A policy that wakes devices runs on a schedule: newly enrolled devices are online.";
  if (draft.trigger === "enroll" && draft.target === "devices") return "A policy for newly enrolled devices runs on every device or on companies.";
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

// The request body that saves the draft. Call only when it has no problem.
export function policyInput(draft: PolicyDraft) {
  const body: Record<string, unknown> = {
    name: draft.name.trim(),
    enabled: draft.enabled,
    action: draft.action,
    target: draft.target,
    device_ids: draft.target === "devices" ? draft.deviceIds : [],
    company_ids: draft.target === "companies" ? draft.companyIds : [],
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
