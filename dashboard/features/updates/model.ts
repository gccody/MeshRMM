// Operating system updates as the server sends them, and how the website
// words, sorts and filters them. See docs/patch-management.md. Imports carry
// the .ts extension so node:test can load this module directly.
import type { DevicePlatform } from "../agents/types.ts";

export type UpdateManager = "windows_update" | "softwareupdate" | "apt" | "dnf" | "yum" | "pacman";

export type UpdateCategory =
  | "security"
  | "critical"
  | "definition"
  | "driver"
  | "feature_pack"
  | "service_pack"
  | "update_rollup"
  | "upgrade"
  | "tool"
  | "other";

export type UpdateSeverity = "critical" | "important" | "moderate" | "low";
export type RebootBehavior = "never" | "if_required" | "always";
export type RunKind = "scan" | "install";
export type RunStatus = "pending" | "succeeded" | "partial" | "failed" | "lost";
export type ItemOutcome = "installed" | "failed";

// An update a device doesn't have yet.
export type PendingUpdate = {
  id: string;
  title: string;
  kb: string | null;
  version: string | null;
  current_version: string | null;
  category: UpdateCategory;
  severity: UpdateSeverity | null;
  size_bytes: number | null;
  reboot_required: boolean;
  released_at: number | null;
  source: string | null;
  // When the server first saw it pending.
  first_seen_at: number;
};

export type UpdateRunItem = {
  update_id: string;
  title: string;
  kb: string | null;
  category: UpdateCategory;
  outcome: ItemOutcome;
  error: string | null;
};

// A scan or install the server asked a device for.
export type UpdateRun = {
  id: string;
  device_id: string;
  device_name: string | null;
  kind: RunKind;
  status: RunStatus;
  progress: string | null;
  progress_percent: number | null;
  update_ids: string[];
  categories: UpdateCategory[];
  reboot: RebootBehavior | null;
  reboot_delay_minutes: number | null;
  requested_by: string;
  policy_name: string | null;
  job_id: string | null;
  error: string | null;
  installed: number;
  failed: number;
  reboot_required: boolean;
  reboot_scheduled_at: number | null;
  reboot_error: string | null;
  created_at: number;
  completed_at: number | null;
  // Installs only; events leave them out.
  items?: UpdateRunItem[];
};

// GET /v1/agents/{id}/updates: where the device stands.
export type UpdateStatus = {
  manager: UpdateManager | null;
  // False once a scan said the device has no update manager MeshRMM supports.
  supported: boolean;
  // False where only every pending update can be installed at once (pacman).
  can_select: boolean;
  scanned_at: number | null;
  received_at: number | null;
  scan_error: string | null;
  reboot_pending: boolean;
  reboot_pending_since: number | null;
  scanning: { run_id: string; created_at: number } | null;
  installing: UpdateRun | null;
  pending: number;
  security: number;
  critical: number;
  oldest_pending_at: number | null;
};

export type DeviceUpdates = {
  device_id: string;
  status: UpdateStatus;
  updates: PendingUpdate[];
};

// GET /v1/updates/devices: one device of the fleet.
export type DeviceUpdateSummary = {
  device_id: string;
  device_name: string;
  company_id: string | null;
  // The device's tags, from the live inventory; the server doesn't send them.
  tag_ids?: string[];
  platform: DevicePlatform;
  connected: boolean;
  manager: UpdateManager | null;
  supported: boolean;
  can_select: boolean;
  scanned_at: number | null;
  received_at: number | null;
  scan_error: string | null;
  reboot_pending: boolean;
  reboot_pending_since: number | null;
  pending: number;
  security: number;
  critical: number;
  oldest_pending_at: number | null;
  scanning: boolean;
  installing: boolean;
  last_install: { id: string; status: RunStatus; created_at: number; completed_at: number | null; installed: number; failed: number } | null;
};

// What the event socket sends about updates.
export type UpdateEvent =
  | { type: "device_updates"; device: DeviceUpdateSummary }
  | { type: "update_run"; run: UpdateRun }
  | { type: "updates_resync" };

export const MANAGER_LABELS: Record<UpdateManager, string> = {
  windows_update: "Windows Update",
  softwareupdate: "Software Update",
  apt: "APT",
  dnf: "DNF",
  yum: "YUM",
  pacman: "pacman",
};

// In the order the website lists them: the most urgent first.
export const CATEGORIES: readonly UpdateCategory[] = [
  "security",
  "critical",
  "definition",
  "update_rollup",
  "driver",
  "feature_pack",
  "service_pack",
  "upgrade",
  "tool",
  "other",
];

export const CATEGORY_LABELS: Record<UpdateCategory, string> = {
  security: "Security",
  critical: "Critical",
  definition: "Definition",
  driver: "Driver",
  feature_pack: "Feature pack",
  service_pack: "Service pack",
  update_rollup: "Update rollup",
  upgrade: "Upgrade",
  tool: "Tool",
  other: "Other",
};

export const SEVERITY_LABELS: Record<UpdateSeverity, string> = {
  critical: "Critical",
  important: "Important",
  moderate: "Moderate",
  low: "Low",
};

export const REBOOT_LABELS: Record<RebootBehavior, string> = {
  never: "Never",
  if_required: "If an update needs it",
  always: "Always",
};

export const REBOOT_HELP: Record<RebootBehavior, string> = {
  never: "The device keeps running; updates that need a restart finish at its next one.",
  if_required: "Restart only when an installed update, or the device, says it needs one.",
  always: "Restart after the install even if nothing asked for it.",
};

export const RUN_STATUS_LABELS: Record<RunStatus, string> = {
  pending: "Running",
  succeeded: "Succeeded",
  partial: "Partly failed",
  failed: "Failed",
  lost: "No result",
};

export const MIN_REBOOT_DELAY_MINUTES = 1;
export const MAX_REBOOT_DELAY_MINUTES = 1440;
export const DEFAULT_REBOOT_DELAY_MINUTES = 5;
// The delays the website offers, in minutes.
export const REBOOT_DELAYS = [1, 5, 15, 30, 60, 120, 240, 480, 1440] as const;

// A device whose last scan is older than this shows its last scan as old.
// The server's "Not scanned recently" filter and tile use the same
// threshold (updates::store::STALE_SCAN_MS).
export const STALE_SCAN_MS = 3 * 24 * 60 * 60_000;

const CATEGORY_ORDER = new Map(CATEGORIES.map((category, index) => [category, index]));
const SEVERITY_ORDER: Record<UpdateSeverity, number> = { critical: 0, important: 1, moderate: 2, low: 3 };

export const categoryLabel = (category: string) => CATEGORY_LABELS[category as UpdateCategory] ?? category;
export const managerLabel = (manager: string | null) => (manager ? MANAGER_LABELS[manager as UpdateManager] ?? manager : null);

// "12 minutes", "3 hours", "5 days": how long something has been so.
export function formatAge(since: number, now = Date.now()) {
  const minutes = Math.max(0, Math.floor((now - since) / 60_000));
  if (minutes < 60) return minutes <= 1 ? "1 minute" : `${minutes} minutes`;
  const hours = Math.floor(minutes / 60);
  if (hours < 48) return hours === 1 ? "1 hour" : `${hours} hours`;
  const days = Math.floor(hours / 24);
  return `${days} days`;
}

// "5 minutes", "2 hours", "1 day".
export function formatDelay(minutes: number) {
  if (minutes < 60) return minutes === 1 ? "1 minute" : `${minutes} minutes`;
  if (minutes % 1440 === 0) return minutes === 1440 ? "1 day" : `${minutes / 1440} days`;
  const hours = minutes / 60;
  return hours === 1 ? "1 hour" : `${hours} hours`;
}

// The most urgent first: by category, then severity, then title.
export function sortPendingUpdates(updates: readonly PendingUpdate[]) {
  return [...updates].sort((left, right) =>
    (CATEGORY_ORDER.get(left.category) ?? 99) - (CATEGORY_ORDER.get(right.category) ?? 99)
    || (left.severity ? SEVERITY_ORDER[left.severity] : 9) - (right.severity ? SEVERITY_ORDER[right.severity] : 9)
    || left.title.localeCompare(right.title, undefined, { sensitivity: "base" })
    || left.id.localeCompare(right.id));
}

// How many pending updates each category has, in list order, without empty ones.
export function categoryCounts(updates: readonly PendingUpdate[]) {
  const counts = new Map<UpdateCategory, number>();
  for (const update of updates) counts.set(update.category, (counts.get(update.category) ?? 0) + 1);
  return CATEGORIES.filter((category) => counts.has(category)).map((category) => ({ category, count: counts.get(category) ?? 0 }));
}

// "openssl 3.0.13 → 3.0.14", "KB5034441", in a few words.
export function updateDetail(update: Pick<PendingUpdate, "kb" | "version" | "current_version" | "source">) {
  const version = update.version
    ? update.current_version ? `${update.current_version} → ${update.version}` : update.version
    : null;
  return [update.kb, version, update.source].filter(Boolean).join(" · ");
}

// Which updates an install covers, in words.
export function describeSelection(selection: { update_ids?: readonly string[]; categories?: readonly UpdateCategory[] }) {
  const ids = selection.update_ids ?? [];
  if (ids.length) return ids.length === 1 ? "1 chosen update" : `${ids.length} chosen updates`;
  const categories = CATEGORIES.filter((category) => (selection.categories ?? []).includes(category));
  if (!categories.length) return "All updates";
  const names = categories.map((category) => CATEGORY_LABELS[category].toLowerCase());
  const list = names.length === 1 ? names[0] : `${names.slice(0, -1).join(", ")} and ${names.at(-1)}`;
  return `${list[0].toUpperCase()}${list.slice(1)} updates`;
}

// When the device restarts after an install, in words.
export function describeReboot(reboot: RebootBehavior | null, delayMinutes: number | null) {
  if (!reboot || reboot === "never") return "Doesn’t restart";
  const delay = delayMinutes ? `, ${formatDelay(delayMinutes)} after warning the user` : "";
  return reboot === "always" ? `Restarts${delay}` : `Restarts if needed${delay}`;
}

export type RunTone = "pending" | "success" | "problem" | "muted";

export function runTone(status: RunStatus): RunTone {
  switch (status) {
    case "pending": return "pending";
    case "succeeded": return "success";
    case "lost": return "muted";
    default: return "problem";
  }
}

// "3 installed · 1 failed", or what happened instead.
export function runSummary(run: Pick<UpdateRun, "kind" | "status" | "installed" | "failed" | "error">) {
  if (run.status === "pending") return run.kind === "scan" ? "Scanning" : "Installing";
  if (run.status === "lost") return "The device never reported back";
  if (run.kind === "scan") return run.status === "succeeded" ? "Scanned" : run.error ?? "The scan failed";
  const parts = [];
  if (run.installed) parts.push(`${run.installed} installed`);
  if (run.failed) parts.push(`${run.failed} failed`);
  if (!parts.length) return run.error ?? (run.status === "succeeded" ? "Nothing was pending" : "Nothing installed");
  return parts.join(" · ");
}

// Who started a run: a policy, or a user.
export const runStarter = (run: Pick<UpdateRun, "policy_name" | "requested_by">) =>
  run.policy_name ? `Policy “${run.policy_name}”` : run.requested_by;

// What the restart after an install did, or null when none was asked for.
export function runRebootText(run: Pick<UpdateRun, "status" | "reboot" | "reboot_required" | "reboot_scheduled_at" | "reboot_error">, formatTime: (unixMs: number) => string) {
  if (run.status === "pending") return null;
  if (run.reboot_error) return `Couldn’t restart: ${run.reboot_error}`;
  if (run.reboot_scheduled_at) return `Restart set for ${formatTime(run.reboot_scheduled_at)}`;
  if (run.reboot_required) return "Needs a restart to finish";
  return null;
}

// ── Choosing updates ───────────────────────────────────────

// The chosen IDs that are still pending.
export function chosenPending(updates: readonly PendingUpdate[], chosen: ReadonlySet<string>) {
  return updates.filter((update) => chosen.has(update.id)).map((update) => update.id);
}

// Why the restart delay can't be used, or null.
export function rebootDelayProblem(minutes: number) {
  return Number.isInteger(minutes) && minutes >= MIN_REBOOT_DELAY_MINUTES && minutes <= MAX_REBOOT_DELAY_MINUTES
    ? null
    : `Warn users ${MIN_REBOOT_DELAY_MINUTES} minute to ${MAX_REBOOT_DELAY_MINUTES / 60} hours before restarting.`;
}

export type InstallChoice = {
  updateIds: readonly string[];
  categories: readonly UpdateCategory[];
  reboot: RebootBehavior;
  rebootDelayMinutes: number;
};

// POST /v1/agents/{id}/updates/install's body.
export function installBody(choice: InstallChoice) {
  return {
    update_ids: [...choice.updateIds],
    categories: choice.updateIds.length ? [] : [...choice.categories],
    reboot: choice.reboot,
    reboot_delay_minutes: choice.rebootDelayMinutes,
  };
}

// ── The fleet ──────────────────────────────────────────────

export type FleetStatusFilter = "" | "pending" | "security" | "failed" | "reboot" | "stale";

export const FLEET_STATUS_LABELS: Record<Exclude<FleetStatusFilter, "">, string> = {
  pending: "Updates pending",
  security: "Security updates pending",
  failed: "Last install failed",
  reboot: "Restart pending",
  stale: "Not scanned recently",
};

export type FleetFilters = {
  query: string;
  // A company ID, NO_COMPANY_FILTER, or "" for every company.
  company: string;
  // A tag ID, or "" for every device.
  tag: string;
  platform: DevicePlatform | "";
  status: FleetStatusFilter;
};

export const NO_COMPANY_FILTER = "none";
export const DEFAULT_FLEET_FILTERS: FleetFilters = { query: "", company: "", tag: "", platform: "", status: "" };

const PLATFORMS: readonly string[] = ["windows", "macos", "linux"];
const STATUSES: readonly string[] = Object.keys(FLEET_STATUS_LABELS);

export function parseFleetFilters(search: URLSearchParams): FleetFilters {
  const platform = search.get("os") ?? "";
  const status = search.get("status") ?? "";
  return {
    query: search.get("q") ?? "",
    company: search.get("company") ?? "",
    tag: search.get("tag") ?? "",
    platform: PLATFORMS.includes(platform) ? (platform as DevicePlatform) : "",
    status: STATUSES.includes(status) ? (status as FleetStatusFilter) : "",
  };
}

// The filters as the page's search string: only those that are set.
export function fleetFilterParams(filters: FleetFilters) {
  const params = new URLSearchParams();
  if (filters.query.trim()) params.set("q", filters.query.trim());
  if (filters.company) params.set("company", filters.company);
  if (filters.tag) params.set("tag", filters.tag);
  if (filters.platform) params.set("os", filters.platform);
  if (filters.status) params.set("status", filters.status);
  return params;
}

// Whether the device hasn't reported a scan for a while, or never.
export const isStale = (device: Pick<DeviceUpdateSummary, "received_at" | "supported">, now: number) =>
  device.supported && (device.received_at === null || now - device.received_at > STALE_SCAN_MS);

// How many devices the fleet has, and how many are in each state.
export type FleetTotals = { devices: number; pending: number; security: number; failed: number; reboot: number; stale: number };

// GET /v1/updates/devices: a page of the devices the filters match, most
// urgent first, how many match, and the whole fleet's totals.
export type FleetPage = { devices: DeviceUpdateSummary[]; matching: number; totals: FleetTotals };

export const FLEET_PAGE_SIZE = 100;

// GET /v1/updates/devices's address for the filters' page from `offset`;
// a `limit` of 0 asks for the counts alone.
export function fleetPagePath(filters: FleetFilters, offset: number, limit = FLEET_PAGE_SIZE) {
  const params = new URLSearchParams();
  const query = filters.query.trim();
  if (query) params.set("query", query);
  if (filters.company) params.set("company", filters.company);
  if (filters.tag) params.set("tag", filters.tag);
  if (filters.platform) params.set("platform", filters.platform);
  if (filters.status) params.set("status", filters.status);
  params.set("limit", String(limit));
  if (offset > 0) params.set("offset", String(offset));
  return `/v1/updates/devices?${params}`;
}

// The filters a list is loaded for, as one string to compare.
export const fleetKey = (filters: FleetFilters) => fleetPagePath(filters, 0, 0);

// The pages loaded so far for one set of filters (`key`), with the newest
// counts. Requests are numbered as they're sent: `listTicket` is the one
// whose first page the list starts with, `countsTicket` the one whose
// counts it shows.
export type FleetList = {
  key: string;
  devices: DeviceUpdateSummary[];
  matching: number;
  totals: FleetTotals;
  listTicket: number;
  countsTicket: number;
};

// The server's answer to request `ticket` for the filters `key`: a page
// from `offset`, or the counts alone.
export type FleetAnswer =
  | { kind: "page"; key: string; ticket: number; offset: number; page: FleetPage }
  | { kind: "counts"; key: string; ticket: number; page: FleetPage };

// The list after an answer, while the page wants the filters `wanted`. A
// newer request's answer always wins: an older one changes nothing it
// already shows. A page for other filters than those wanted is dropped,
// and a further page joins only the list it was asked for; the fleet's
// totals are the same whatever the filters.
export function applyFleetAnswer(list: FleetList | null, answer: FleetAnswer, wanted: string): FleetList | null {
  const { page, ticket } = answer;
  const withCounts = (current: FleetList): FleetList => ticket > current.countsTicket
    ? { ...current, totals: page.totals, matching: answer.key === current.key ? page.matching : current.matching, countsTicket: ticket }
    : current;
  if (answer.kind === "counts") return list && withCounts(list);
  if (answer.key !== wanted) return list;
  if (answer.offset === 0) {
    if (list && list.key === answer.key && ticket < list.listTicket) return list;
    const fresh = { key: answer.key, devices: page.devices, matching: page.matching, totals: page.totals, listTicket: ticket, countsTicket: ticket };
    if (!list || list.countsTicket < ticket) return fresh;
    return { ...fresh, totals: list.totals, countsTicket: list.countsTicket, matching: list.key === answer.key ? list.matching : page.matching };
  }
  if (!list || list.key !== answer.key || ticket < list.listTicket) return list;
  // The fleet may have moved between pages: a device already loaded stays
  // where it is.
  const loaded = new Set(list.devices.map((device) => device.device_id));
  return withCounts({ ...list, devices: [...list.devices, ...page.devices.filter((device) => !loaded.has(device.device_id))] });
}

// A device's summary from the socket in place of the loaded copy. A
// device not loaded stays out: where it belongs is for the server to say
// when the next page or the next load asks.
export function patchFleetRow(list: FleetList | null, changed: DeviceUpdateSummary): FleetList | null {
  if (!list) return list;
  const index = list.devices.findIndex((device) => device.device_id === changed.device_id);
  if (index === -1) return list;
  const devices = [...list.devices];
  devices[index] = changed;
  return { ...list, devices };
}

// Chosen devices by ID, with the summary they were chosen with, so the
// selection outlives the pages and filters it was made in.
export type ChosenDevices = ReadonlyMap<string, DeviceUpdateSummary>;

// The choice with `devices` chosen, or not.
export function chooseDevices(chosen: ChosenDevices, devices: readonly DeviceUpdateSummary[], checked: boolean): ChosenDevices {
  const next = new Map(chosen);
  for (const device of devices) {
    if (checked) next.set(device.device_id, device);
    else next.delete(device.device_id);
  }
  return next;
}

// The chosen devices with the newer summaries among `devices`; the same
// choice when none of them is chosen.
export function refreshChosen(chosen: ChosenDevices, devices: readonly DeviceUpdateSummary[]): ChosenDevices {
  const changed = devices.filter((device) => chosen.has(device.device_id));
  if (!changed.length) return chosen;
  const next = new Map(chosen);
  for (const device of changed) next.set(device.device_id, device);
  return next;
}

// How long to wait before reading the counts again, the last read having
// been at `lastAt`: at most once every `intervalMs` while changes arrive.
export const countsRefreshDelay = (lastAt: number, now: number, intervalMs = 10_000) => Math.max(0, lastAt + intervalMs - now);

// A run from the socket in place of the history's copy, newest first. The
// event leaves out an install's updates, so the copy's are kept.
export function applyRunUpdate(runs: readonly UpdateRun[], changed: UpdateRun) {
  const previous = runs.find((run) => run.id === changed.id);
  const merged = { ...changed, items: changed.items ?? previous?.items };
  return [merged, ...runs.filter((run) => run.id !== changed.id)]
    .sort((left, right) => right.created_at - left.created_at || right.id.localeCompare(left.id));
}

// ── Socket events ──────────────────────────────────────────

const isObject = (value: unknown): value is Record<string, unknown> => typeof value === "object" && value !== null;

function isSummary(value: unknown): value is DeviceUpdateSummary {
  return isObject(value) && typeof value.device_id === "string" && typeof value.pending === "number";
}

function isRun(value: unknown): value is UpdateRun {
  return isObject(value) && typeof value.id === "string" && typeof value.device_id === "string" && typeof value.status === "string" && typeof value.kind === "string";
}

// An update event from the socket, or null for anything else.
export function parseUpdateEvent(event: unknown): UpdateEvent | null {
  if (!isObject(event)) return null;
  if (event.type === "updates_resync") return { type: "updates_resync" };
  if (event.type === "device_updates" && isSummary(event.device)) return { type: "device_updates", device: event.device };
  if (event.type === "update_run" && isRun(event.run)) return { type: "update_run", run: event.run };
  return null;
}

// ── Policies and jobs ──────────────────────────────────────

// What an update job or policy installs, as its name says it.
export function installName(categories: readonly UpdateCategory[]) {
  return categories.length ? `Install ${describeSelection({ categories }).toLowerCase()}` : "Install all updates";
}
