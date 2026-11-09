// Monitors the Agent checks and self-healing, as the server sends them, the
// rule editor's fields for them, and how the website words them. See
// docs/monitors.md. Imports carry the .ts extension so node:test can load
// this module directly.
import type { Severity } from "../alerts/model.ts";

export type MonitorCondition =
  | "service"
  | "process"
  | "event_log"
  | "ping"
  | "tcp_port"
  | "http"
  | "certificate"
  | "script_check";

export const MONITOR_CONDITION_LABELS: Record<MonitorCondition, string> = {
  service: "Service not running",
  process: "Process check",
  event_log: "Event log entries",
  ping: "Ping fails",
  tcp_port: "Port not answering",
  http: "Website check fails",
  certificate: "Certificate expiring",
  script_check: "Script check fails",
};

export const MONITOR_CONDITION_HELP: Record<MonitorCondition, string> = {
  service: "The device's Agent checks that a Windows service, systemd unit or launchd job is running, and optionally that it starts automatically.",
  process: "The device's Agent checks whether a program is running, or how much CPU or memory it uses.",
  event_log: "The device's Agent looks for new entries in the Windows Event Log, the systemd journal or the macOS unified log since its last check.",
  ping: "The device's Agent pings a host on its network.",
  tcp_port: "The device's Agent connects to a TCP port, such as a database or mail server's.",
  http: "The device's Agent requests an http:// or https:// address and checks the answer.",
  certificate: "The device's Agent reads the TLS certificate a server presents and checks when it expires.",
  script_check: "The device's Agent runs a shared toolbox script: exiting with 0 passes, anything else, or running out of time, fails. Its output is kept with the alert.",
};

export const MONITOR_CONDITIONS = Object.keys(MONITOR_CONDITION_LABELS) as MonitorCondition[];

// Checks that reach past the device's own services and processes: they run
// with the access of whoever saves the rule, so saving one needs
// scripts.run, as the server checks.
const OWNER_ACCESS_REASONS: Partial<Record<MonitorCondition, string>> = {
  event_log: "reads the device’s logs",
  ping: "probes hosts from inside the device’s network",
  tcp_port: "probes hosts from inside the device’s network",
  http: "requests pages from inside the device’s network",
  certificate: "probes hosts from inside the device’s network",
  script_check: "runs a script on the device",
};

export const needsScriptsRun = (condition: string): boolean => Object.hasOwn(OWNER_ACCESS_REASONS, condition);

// Why a condition needs scripts.run, in words; null when it doesn't.
export const scriptsRunReason = (condition: string): string | null => {
  const reason = needsScriptsRun(condition) ? OWNER_ACCESS_REASONS[condition as MonitorCondition] : undefined;
  return reason ? `This check ${reason}, so it runs with the access of whoever saves the rule, and saving it needs the scripts.run permission.` : null;
};

export const isMonitorCondition = (condition: string): condition is MonitorCondition =>
  Object.hasOwn(MONITOR_CONDITION_LABELS, condition);

export type ProcessAlert = "not_running" | "running" | "cpu_above" | "memory_above";
export type EventLevel = "critical" | "error" | "warning" | "information";
export type RunAs = "system" | "user";

export const PROCESS_ALERT_LABELS: Record<ProcessAlert, string> = {
  not_running: "It isn't running",
  running: "It's running",
  cpu_above: "It uses too much CPU",
  memory_above: "It uses too much memory",
};

export const EVENT_LEVELS: readonly EventLevel[] = ["critical", "error", "warning", "information"];
export const EVENT_LEVEL_LABELS: Record<EventLevel, string> = {
  critical: "Critical",
  error: "Error",
  warning: "Warning",
  information: "Information",
};

export const RUN_AS_LABELS: Record<RunAs, string> = {
  system: "SYSTEM or root",
  user: "The signed-in user",
};

export type MonitorCheck =
  | { kind: "service"; name: string; automatic: boolean }
  | { kind: "process"; name: string; alert_when: ProcessAlert; percent?: number | null; megabytes?: number | null; minutes?: number | null }
  | { kind: "event_log"; log: string; source: string; event_ids: number[]; levels: EventLevel[]; message: string }
  | { kind: "ping"; host: string }
  | { kind: "tcp_port"; host: string; port: number }
  | { kind: "http"; url: string; status: number; keyword: string; verify_tls: boolean }
  | { kind: "certificate"; host: string; port: number; days: number }
  | { kind: "script_check"; script_id: string; run_as: RunAs };

export type MonitorSettings = {
  interval_seconds: number;
  timeout_seconds: number;
  // Failed checks in a row that raise the alert.
  failures: number;
  check: MonitorCheck;
};

export type RemediationKind = "restart_service" | "end_process" | "script";

export type RemediationSettings = {
  action: RemediationKind;
  script_id?: string | null;
  run_as?: RunAs | null;
  // At most this many attempts per alert, this many minutes apart.
  attempts: number;
  retry_minutes: number;
};

// When a rule's channels hear of an alert: as soon as it's raised, or only
// once self-healing has given up.
export type NotifyWhen = "raised" | "remediation_failed";
export type RemediationState = "waiting" | "running" | "failed" | "succeeded";

// A monitor's latest result on a device.
export type MonitorState = {
  rule_id: string;
  device_id: string;
  checked_at: number;
  received_at: number;
  passing: boolean;
  failures: number;
  value: number | null;
  detail: string;
  output: string;
};

// GET /v1/agents/{id}/monitors
export type DeviceMonitor = {
  rule_id: string;
  rule_name: string;
  condition: MonitorCondition;
  severity: Severity;
  description: string;
  interval_seconds: number;
  failures_before_alert: number;
  remediation: string | null;
  // Why the device doesn't run it, if it doesn't.
  problem: string | null;
  state: MonitorState | null;
  alert_id: string | null;
  alert_state: string | null;
  remediation_state: RemediationState | null;
};

export type AttemptStatus = "running" | "succeeded" | "failed" | "lost" | "skipped";

// GET /v1/alerts/{id}/remediations
export type RemediationAttempt = {
  id: string;
  alert_id: string;
  attempt: number;
  action: RemediationKind;
  target: string;
  run_as: RunAs | null;
  script_run_id: string | null;
  status: AttemptStatus;
  exit_code: number | null;
  detail: string;
  output: string;
  started_at: number;
  finished_at: number | null;
};

export type MonitorUpdate = { type: "monitor_update"; device_id: string; states: MonitorState[] };

const isObject = (value: unknown): value is Record<string, unknown> => typeof value === "object" && value !== null;

function isMonitorState(value: unknown): value is MonitorState {
  return isObject(value)
    && typeof value.rule_id === "string"
    && typeof value.device_id === "string"
    && typeof value.passing === "boolean"
    && typeof value.checked_at === "number";
}

// A monitor_update event, or null for anything else.
export function parseMonitorUpdate(event: unknown): MonitorUpdate | null {
  if (!isObject(event) || event.type !== "monitor_update" || typeof event.device_id !== "string" || !Array.isArray(event.states)) return null;
  if (!event.states.every(isMonitorState)) return null;
  return { type: "monitor_update", device_id: event.device_id, states: event.states };
}

// The device's monitors with the latest results applied.
export function applyMonitorUpdate(monitors: readonly DeviceMonitor[], update: MonitorUpdate): DeviceMonitor[] {
  const latest = new Map(update.states.map((state) => [state.rule_id, state]));
  return monitors.map((monitor) => {
    const state = latest.get(monitor.rule_id);
    return state ? { ...monitor, state } : monitor;
  });
}

// "30 seconds", "5 minutes", "1 hour".
export function formatSeconds(seconds: number) {
  const plural = (count: number, unit: string) => `${count} ${unit}${count === 1 ? "" : "s"}`;
  if (seconds % 3600 === 0) return plural(seconds / 3600, "hour");
  if (seconds % 60 === 0) return plural(seconds / 60, "minute");
  return plural(seconds, "second");
}

export type MonitorStatus = { label: string; tone: "success" | "problem" | "pending" | "muted" };

// What a device's monitor last found, in a word or two.
export function monitorStatus(monitor: Pick<DeviceMonitor, "problem" | "state" | "failures_before_alert">): MonitorStatus {
  if (monitor.problem) return { label: "Not checked here", tone: "muted" };
  const { state } = monitor;
  if (!state) return { label: "Not checked yet", tone: "pending" };
  if (state.passing) return { label: "Passing", tone: "success" };
  if (state.failures < monitor.failures_before_alert) {
    return { label: `Failing (${state.failures} of ${monitor.failures_before_alert})`, tone: "pending" };
  }
  return { label: "Failing", tone: "problem" };
}

export const REMEDIATION_STATE_LABELS: Record<RemediationState, string> = {
  waiting: "Self-healing waiting",
  running: "Self-healing running",
  failed: "Self-healing failed",
  succeeded: "Fixed by self-healing",
};

export const ATTEMPT_STATUS_LABELS: Record<AttemptStatus, string> = {
  running: "Running",
  succeeded: "Succeeded",
  failed: "Failed",
  lost: "Never reported",
  skipped: "Skipped",
};

export function attemptTone(status: AttemptStatus): "success" | "problem" | "pending" {
  if (status === "succeeded") return "success";
  if (status === "running") return "pending";
  return "problem";
}

export const REMEDIATION_LABELS: Record<RemediationKind, string> = {
  restart_service: "Restart the service",
  end_process: "End the process",
  script: "Run a shared script",
};

// The intervals a monitor can check at.
export const INTERVAL_CHOICES: readonly number[] = [30, 60, 120, 300, 600, 900, 1800, 3600, 6 * 3600, 12 * 3600, 24 * 3600];

export const MAX_FAILURES = 100;
export const MAX_EVENT_IDS = 32;
export const MAX_CERTIFICATE_DAYS = 365;
export const MAX_PROCESS_MINUTES = 1440;
export const MAX_PROCESS_MEGABYTES = 1_048_576;
export const MAX_ATTEMPTS = 10;
export const MAX_RETRY_MINUTES = 1440;

const timeoutRange = (condition: MonitorCondition) => (condition === "script_check" ? { min: 10, max: 3600 } : { min: 1, max: 300 });

// The rule editor's monitor fields, as text while being edited.
export type MonitorForm = {
  interval: number;
  timeout: string;
  failures: string;
  name: string;
  automatic: boolean;
  alertWhen: ProcessAlert;
  percent: string;
  megabytes: string;
  minutes: string;
  log: string;
  source: string;
  eventIds: string;
  levels: EventLevel[];
  message: string;
  host: string;
  port: string;
  url: string;
  status: string;
  keyword: string;
  verifyTls: boolean;
  days: string;
  scriptId: string;
  runAs: RunAs;
};

export function monitorForm(monitor: MonitorSettings | null | undefined, condition: MonitorCondition): MonitorForm {
  const form: MonitorForm = {
    interval: 300,
    timeout: condition === "script_check" ? "60" : "30",
    failures: condition === "ping" || condition === "tcp_port" || condition === "http" ? "2" : "1",
    name: "",
    automatic: false,
    alertWhen: "not_running",
    percent: "90",
    megabytes: "1024",
    minutes: "5",
    log: "System",
    source: "",
    eventIds: "",
    levels: ["critical", "error"],
    message: "",
    host: "",
    port: condition === "certificate" ? "443" : "",
    url: "",
    status: "200",
    keyword: "",
    verifyTls: true,
    days: "14",
    scriptId: "",
    runAs: "system",
  };
  if (!monitor || monitor.check.kind !== condition) return form;
  form.interval = monitor.interval_seconds;
  form.timeout = String(monitor.timeout_seconds);
  form.failures = String(monitor.failures);
  const { check } = monitor;
  switch (check.kind) {
    case "service":
      return { ...form, name: check.name, automatic: check.automatic };
    case "process":
      return {
        ...form,
        name: check.name,
        alertWhen: check.alert_when,
        percent: check.percent == null ? form.percent : String(check.percent),
        megabytes: check.megabytes == null ? form.megabytes : String(check.megabytes),
        minutes: check.minutes == null ? form.minutes : String(check.minutes),
      };
    case "event_log":
      return { ...form, log: check.log, source: check.source, eventIds: check.event_ids.join(", "), levels: check.levels, message: check.message };
    case "ping":
      return { ...form, host: check.host };
    case "tcp_port":
      return { ...form, host: check.host, port: String(check.port) };
    case "http":
      return { ...form, url: check.url, status: String(check.status), keyword: check.keyword, verifyTls: check.verify_tls };
    case "certificate":
      return { ...form, host: check.host, port: String(check.port), days: String(check.days) };
    case "script_check":
      return { ...form, scriptId: check.script_id, runAs: check.run_as };
  }
}

const integer = (text: string) => (/^\s*\d+\s*$/.test(text) ? Number(text) : Number.NaN);
const within = (value: number, min: number, max: number) => Number.isInteger(value) && value >= min && value <= max;

// The event IDs typed as "7, 11, 51", or null if they aren't.
export function parseEventIds(text: string): number[] | null {
  const parts = text.split(/[\s,]+/).filter(Boolean);
  const ids = parts.map(integer);
  if (ids.some((id) => !within(id, 0, 65_535))) return null;
  return [...new Set(ids)].sort((left, right) => left - right);
}

const HOST = /^(?:\[[0-9a-fA-F:.%\w-]+\]|[0-9a-fA-F:.%\w-]+)$/;

// What keeps the monitor fields from saving, or null.
export function monitorFormProblem(form: MonitorForm, condition: MonitorCondition): string | null {
  const timeout = integer(form.timeout);
  const { min, max } = timeoutRange(condition);
  if (!within(timeout, min, max)) return `Enter a timeout of ${min} to ${max.toLocaleString()} seconds.`;
  if (!within(integer(form.failures), 1, MAX_FAILURES)) return `Enter 1 to ${MAX_FAILURES} failed checks in a row.`;
  switch (condition) {
    case "service":
      if (!form.name.trim() || form.name.trim().startsWith("-")) return "Enter the service's name.";
      return null;
    case "process":
      if (!form.name.trim() || form.name.trim().startsWith("-")) return "Enter the process's file name.";
      if (/[/\\]/.test(form.name)) return "Enter the process's file name, without a folder.";
      if (form.alertWhen === "cpu_above" && !within(integer(form.percent), 1, 99)) return "Enter a CPU threshold of 1 to 99%.";
      if (form.alertWhen === "memory_above" && !within(integer(form.megabytes), 1, MAX_PROCESS_MEGABYTES)) return "Enter a memory threshold in MB.";
      if ((form.alertWhen === "cpu_above" || form.alertWhen === "memory_above") && !within(integer(form.minutes), 1, MAX_PROCESS_MINUTES)) {
        return `Enter 1 to ${MAX_PROCESS_MINUTES.toLocaleString()} minutes.`;
      }
      return null;
    case "event_log": {
      const ids = parseEventIds(form.eventIds);
      if (!ids) return "Enter event IDs from 0 to 65535, separated by commas.";
      if (ids.length > MAX_EVENT_IDS) return `Choose at most ${MAX_EVENT_IDS} event IDs.`;
      if (/['"<>&]/.test(form.source)) return "The source can't contain quotes, < > or &.";
      if (/['"<>&*?]/.test(form.log)) return "The log name can't contain quotes, wildcards, < > or &.";
      return null;
    }
    case "ping":
    case "tcp_port":
    case "certificate":
      if (!HOST.test(form.host.trim())) return "Enter a host name or IP address.";
      if (condition !== "ping" && !within(integer(form.port), 1, 65_535)) return "Enter a port from 1 to 65535.";
      if (condition === "certificate" && !within(integer(form.days), 1, MAX_CERTIFICATE_DAYS)) return `Enter 1 to ${MAX_CERTIFICATE_DAYS} days.`;
      return null;
    case "http":
      if (!/^https?:\/\/[^\s/?#]+/i.test(form.url.trim())) return "Enter an address starting with http:// or https://.";
      if (!within(integer(form.status), 100, 599)) return "Enter an HTTP status from 100 to 599.";
      return null;
    case "script_check":
      return form.scriptId ? null : "Choose the script to run.";
  }
}

// The monitor fields as the server takes them.
export function monitorInput(form: MonitorForm, condition: MonitorCondition): MonitorSettings {
  const base = { interval_seconds: form.interval, timeout_seconds: integer(form.timeout), failures: integer(form.failures) };
  const check = ((): MonitorCheck => {
    switch (condition) {
      case "service":
        return { kind: "service", name: form.name.trim(), automatic: form.automatic };
      case "process": {
        const load = form.alertWhen === "cpu_above" || form.alertWhen === "memory_above";
        return {
          kind: "process",
          name: form.name.trim(),
          alert_when: form.alertWhen,
          percent: form.alertWhen === "cpu_above" ? integer(form.percent) : null,
          megabytes: form.alertWhen === "memory_above" ? integer(form.megabytes) : null,
          minutes: load ? integer(form.minutes) : null,
        };
      }
      case "event_log":
        return { kind: "event_log", log: form.log.trim(), source: form.source.trim(), event_ids: parseEventIds(form.eventIds) ?? [], levels: form.levels, message: form.message.trim() };
      case "ping":
        return { kind: "ping", host: form.host.trim() };
      case "tcp_port":
        return { kind: "tcp_port", host: form.host.trim(), port: integer(form.port) };
      case "http":
        return { kind: "http", url: form.url.trim(), status: integer(form.status), keyword: form.keyword, verify_tls: form.verifyTls };
      case "certificate":
        return { kind: "certificate", host: form.host.trim(), port: integer(form.port), days: integer(form.days) };
      case "script_check":
        return { kind: "script_check", script_id: form.scriptId, run_as: form.runAs };
    }
  })();
  return { ...base, check };
}

// Conditions whose alerts self-healing can't act on: the device can't be
// reached, or a person has to look.
const NOT_HEALABLE = new Set(["device_offline", "script_failed", "update_install_failed", "app_install_failed"]);

// The actions a rule with `condition` can take to heal its alerts.
export function remediationChoices(condition: string, alertWhen: ProcessAlert): RemediationKind[] {
  if (NOT_HEALABLE.has(condition)) return [];
  const choices: RemediationKind[] = [];
  if (condition === "service") choices.push("restart_service");
  if (condition === "process" && alertWhen !== "not_running") choices.push("end_process");
  choices.push("script");
  return choices;
}

// The rule editor's self-healing fields.
export type RemediationForm = {
  enabled: boolean;
  action: RemediationKind;
  scriptId: string;
  runAs: RunAs;
  attempts: string;
  retryMinutes: string;
  notifyWhen: NotifyWhen;
};

export function remediationForm(remediation: RemediationSettings | null | undefined, notifyWhen: NotifyWhen | undefined, condition: string): RemediationForm {
  return {
    enabled: Boolean(remediation),
    action: remediation?.action ?? (condition === "service" ? "restart_service" : "script"),
    scriptId: remediation?.script_id ?? "",
    runAs: remediation?.run_as ?? "system",
    attempts: String(remediation?.attempts ?? 3),
    retryMinutes: String(remediation?.retry_minutes ?? 5),
    notifyWhen: notifyWhen ?? "raised",
  };
}

// What keeps the self-healing fields from saving, or null.
export function remediationFormProblem(form: RemediationForm, choices: readonly RemediationKind[]): string | null {
  if (!form.enabled) return null;
  if (!choices.includes(form.action)) return "Choose what self-healing does.";
  if (form.action === "script" && !form.scriptId) return "Choose the script self-healing runs.";
  if (!within(integer(form.attempts), 1, MAX_ATTEMPTS)) return `Enter 1 to ${MAX_ATTEMPTS} attempts.`;
  if (!within(integer(form.retryMinutes), 1, MAX_RETRY_MINUTES)) return `Enter 1 to ${MAX_RETRY_MINUTES.toLocaleString()} minutes between attempts.`;
  return null;
}

// The self-healing fields as the server takes them.
export function remediationInput(form: RemediationForm, choices: readonly RemediationKind[]) {
  if (!form.enabled || !choices.includes(form.action)) return { remediation: null, notify_when: "raised" as NotifyWhen };
  const script = form.action === "script";
  return {
    remediation: {
      action: form.action,
      script_id: script ? form.scriptId : null,
      run_as: script ? form.runAs : null,
      attempts: integer(form.attempts),
      retry_minutes: integer(form.retryMinutes),
    } satisfies RemediationSettings,
    notify_when: form.notifyWhen,
  };
}
