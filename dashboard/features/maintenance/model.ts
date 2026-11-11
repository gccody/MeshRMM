// Maintenance windows as the server sends them, and how the website words
// and asks for them. See docs/maintenance-windows.md. Imports carry the .ts
// extension so node:test can load this module directly.
import type { Agent, CurrentMaintenance, MaintenanceAutomation } from "../agents/types.ts";
import type { TagMatch } from "../tags/model.ts";

export type WindowState = "upcoming" | "current" | "ended" | "cancelled";

// GET /v1/maintenance-windows, /v1/maintenance-windows/{id}
export type MaintenanceWindow = {
  id: string;
  device_id: string;
  device_name: string;
  company_id: string | null;
  company_name: string | null;
  starts_at: number;
  // Null until someone ends it.
  ends_at: number | null;
  reason: string;
  automation: MaintenanceAutomation;
  // A person's name, or `policy “…”`.
  created_by: string;
  created_by_user_id: string | null;
  policy_id: string | null;
  policy_name: string | null;
  // The update install it covers.
  update_run_id: string | null;
  // Who ended it before its end, if someone did.
  ended_by: string | null;
  state: WindowState;
  created_at: number;
  updated_at: number;
};

export type WindowList = { windows: MaintenanceWindow[]; truncated: boolean };

// GET /v1/agents/{id}/maintenance-windows
export type DeviceWindows = { open: MaintenanceWindow[]; past: MaintenanceWindow[] };

// What the windows list shows.
export type Showing = "open" | "current" | "upcoming" | "past";

export const SHOWING_LABELS: Record<Showing, string> = {
  open: "Current and upcoming",
  current: "Current",
  upcoming: "Upcoming",
  past: "Past",
};

export const WINDOW_STATE_LABELS: Record<WindowState, string> = {
  upcoming: "Upcoming",
  current: "In force",
  ended: "Ended",
  cancelled: "Cancelled",
};

export const AUTOMATION_CHOICES: Record<MaintenanceAutomation, { label: string; help: string }> = {
  skip: { label: "Hold back all scheduled automation", help: "Policies' scheduled runs skip the device: scripts, update scans and installs, application policies and wakes." },
  scans: { label: "Let update scans run", help: "Scheduled update scans still run, so its list of updates stays current; everything else is skipped." },
  run: { label: "Let scheduled automation run", help: "Only alerts and self-healing are held back." },
};

// Whether presence shows the device in maintenance now.
export const inMaintenance = (agent: Pick<Agent, "maintenance">) => Boolean(agent.maintenance?.current);

// "until 05:00" for today, "until 3 Jun, 05:00" later, or "until someone ends it".
export function untilText(endsAt: number | null, now: number, format: { time: (at: number) => string; dateTime: (at: number) => string }) {
  if (endsAt === null) return "until someone ends it";
  const sameDay = new Date(endsAt).toDateString() === new Date(now).toDateString();
  return `until ${sameDay ? format.time(endsAt) : format.dateTime(endsAt)}`;
}

// A device's maintenance in force, in one line: "In maintenance until 05:00:
// Replacing the disk".
export function maintenanceLine(current: CurrentMaintenance, now: number, format: { time: (at: number) => string; dateTime: (at: number) => string }) {
  return `In maintenance ${untilText(current.ends_at, now, format)}: ${current.reason}`;
}

// What a window holds back, briefly.
export function holdsBack(automation: MaintenanceAutomation) {
  switch (automation) {
    case "skip": return "Alerts and scheduled automation are held back.";
    case "scans": return "Alerts and scheduled automation are held back; update scans still run.";
    case "run": return "Alerts are held back; scheduled automation runs.";
  }
}

// "Put Desk in maintenance", or "Put 3 devices in maintenance".
export function maintenanceTitle(names: readonly string[]) {
  return names.length === 1 ? `Put ${names[0]} in maintenance` : `Put ${names.length} devices in maintenance`;
}

// The devices a start or an end covers.
export type MaintenanceTarget =
  | { target: "devices"; device_ids: string[] }
  | { target: "companies"; company_ids: string[] }
  | { target: "tags"; tag_ids: string[]; tag_match: TagMatch }
  | { target: "all" };

// The lengths offered, in minutes.
export const DURATION_CHOICES: readonly { minutes: number; label: string }[] = [
  { minutes: 30, label: "30 minutes" },
  { minutes: 60, label: "1 hour" },
  { minutes: 120, label: "2 hours" },
  { minutes: 240, label: "4 hours" },
  { minutes: 480, label: "8 hours" },
  { minutes: 24 * 60, label: "1 day" },
  { minutes: 7 * 24 * 60, label: "1 week" },
];

export const MAX_REASON_CHARS = 500;
const YEAR_MS = 365 * 24 * 60 * 60_000;

// What the start dialog holds. Times stay as the date and time inputs give
// them, in the browser's time zone.
export type StartDraft = {
  reason: string;
  // Now, or at `startsAt`.
  later: boolean;
  startsAt: string;
  // For a length from `durationMinutes`, until `endsAt`, or until someone ends it.
  length: "duration" | "until" | "open";
  durationMinutes: number;
  endsAt: string;
  automation: MaintenanceAutomation;
};

export const startDraft = (): StartDraft => ({
  reason: "",
  later: false,
  startsAt: "",
  length: "duration",
  durationMinutes: 60,
  endsAt: "",
  automation: "skip",
});

const LOCAL = /^\d{4}-\d{2}-\d{2}T\d{2}:\d{2}$/u;

// A date and time input's value as Unix milliseconds in the browser's time
// zone, or null.
export function parseLocal(text: string): number | null {
  if (!LOCAL.test(text)) return null;
  const at = new Date(text).getTime();
  return Number.isFinite(at) ? at : null;
}

// Why the draft can't start, or null when it can.
export function startDraftProblem(draft: StartDraft, now: number): string | null {
  const reason = draft.reason.trim();
  if (!reason) return "Say why the devices are in maintenance.";
  if (Array.from(reason).length > MAX_REASON_CHARS) return `Keep the reason to ${MAX_REASON_CHARS} characters.`;
  let starts = now;
  if (draft.later) {
    const at = parseLocal(draft.startsAt);
    if (at === null) return "Choose when it starts.";
    if (at < now) return "Choose a start in the future, or start it now.";
    if (at > now + YEAR_MS) return "It can start at most a year ahead.";
    starts = at;
  }
  if (draft.length === "until") {
    const end = parseLocal(draft.endsAt);
    if (end === null) return "Choose when it ends.";
    if (end <= starts) return "It must end after it starts.";
    if (end - starts > YEAR_MS) return "It can last at most a year.";
  }
  return null;
}

// The request body that starts the draft on `target`. Call only when it has
// no problem.
export function startBody(draft: StartDraft, target: MaintenanceTarget) {
  const body: Record<string, unknown> = { ...target, reason: draft.reason.trim(), automation: draft.automation };
  if (draft.later) body.starts_at = parseLocal(draft.startsAt);
  if (draft.length === "duration") body.duration_minutes = draft.durationMinutes;
  if (draft.length === "until") body.ends_at = parseLocal(draft.endsAt);
  return body;
}

// A time as a date and time input's value, in the browser's time zone.
export function localInput(at: number) {
  const date = new Date(at);
  const pad = (value: number) => String(value).padStart(2, "0");
  return `${date.getFullYear()}-${pad(date.getMonth() + 1)}-${pad(date.getDate())}T${pad(date.getHours())}:${pad(date.getMinutes())}`;
}

// Who set a window, and who ended it, in words.
export function windowSetBy(window: Pick<MaintenanceWindow, "created_by" | "ended_by" | "state">) {
  const set = `Set by ${window.created_by}`;
  if (window.state === "cancelled") return `${set}; cancelled${window.ended_by ? ` by ${window.ended_by}` : ""}`;
  return window.ended_by ? `${set}; ended by ${window.ended_by}` : set;
}

// The windows with a changed one in place of its old copy, without those
// a list of open windows no longer shows.
export function replaceWindow(current: readonly MaintenanceWindow[], changed: MaintenanceWindow, showing: Showing) {
  const kept = showing === "past" || showing === "open" && (changed.state === "current" || changed.state === "upcoming") || changed.state === showing;
  return kept ? current.map((window) => window.id === changed.id ? changed : window) : current.filter((window) => window.id !== changed.id);
}

// A list query for `showing`.
export function windowsPath(showing: Showing, filters: { device?: string; company?: string; policy?: string } = {}) {
  const params = new URLSearchParams({ state: showing });
  if (filters.device) params.set("device", filters.device);
  if (filters.company) params.set("company", filters.company);
  if (filters.policy) params.set("policy", filters.policy);
  return `/v1/maintenance-windows?${params}`;
}

// Every device in maintenance now, for the wall's filter.
export const countInMaintenance = (agents: readonly Pick<Agent, "maintenance">[]) => agents.filter(inMaintenance).length;
