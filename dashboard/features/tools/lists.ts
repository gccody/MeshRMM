// The remote tools' lists: processes and services, sorted, filtered and
// drawn as a tree. Only the tools' own chunk loads it. Imports carry the .ts
// extension so node:test can load this module directly.
import type { DevicePlatform } from "../agents/types.ts";

// How often an open, visible processes page reads the list again.
export const PROCESS_REFRESH_MS = 3_000;
// The most processes one request ends, as the server allows.
export const MAX_ENDED_PROCESSES = 256;

export type ProcessInfo = {
  pid: number;
  parent_pid?: number;
  name: string;
  path?: string;
  user?: string;
  cpu_percent: number;
  memory_bytes: number;
  started_at?: number;
  command_line?: string;
  command_line_truncated?: boolean;
  session?: number;
};

export type ProcessList = {
  processes: ProcessInfo[];
  read_at: number;
  logical_cpus: number;
  memory_total_bytes?: number;
  truncated?: boolean;
};

export type ToolError = { code: string; message: string };

export type ProcessesEnded = {
  ended: { pid: number; name: string }[];
  failed: { pid: number; name?: string; error: ToolError }[];
};

export type ProcessSortKey = "name" | "pid" | "user" | "cpu" | "memory" | "started";
export type Sort<K extends string> = { key: K; descending: boolean };

export const DEFAULT_PROCESS_SORT: Sort<ProcessSortKey> = { key: "cpu", descending: true };

// Choosing the sorted column again turns its order around; another column
// starts in its natural order (largest first for numbers).
export function nextSort<K extends string>(current: Sort<K>, key: K, numeric: readonly K[]): Sort<K> {
  if (current.key === key) return { key, descending: !current.descending };
  return { key, descending: numeric.includes(key) };
}

export const NUMERIC_PROCESS_KEYS: readonly ProcessSortKey[] = ["cpu", "memory", "started"];

const collator = new Intl.Collator(undefined, { numeric: true, sensitivity: "base" });

function compareProcesses(a: ProcessInfo, b: ProcessInfo, key: ProcessSortKey) {
  switch (key) {
    case "name":
      return collator.compare(a.name, b.name) || a.pid - b.pid;
    case "pid":
      return a.pid - b.pid;
    case "user":
      return collator.compare(a.user ?? "", b.user ?? "") || a.pid - b.pid;
    case "cpu":
      return a.cpu_percent - b.cpu_percent || b.pid - a.pid;
    case "memory":
      return a.memory_bytes - b.memory_bytes || b.pid - a.pid;
    case "started":
      return (a.started_at ?? 0) - (b.started_at ?? 0) || a.pid - b.pid;
  }
}

export function sortProcesses(processes: readonly ProcessInfo[], sort: Sort<ProcessSortKey>) {
  const sign = sort.descending ? -1 : 1;
  return [...processes].sort((a, b) => sign * compareProcesses(a, b, sort.key));
}

// Whether `process` matches what was typed: its name, ID, user, path or
// command line, ignoring case.
export function processMatches(process: ProcessInfo, query: string) {
  const needle = query.trim().toLowerCase();
  if (!needle) return true;
  return [process.name, String(process.pid), process.user, process.path, process.command_line]
    .some((field) => field?.toLowerCase().includes(needle));
}

// The process a child's parent ID names, unless that ID now belongs to a
// process started after the child: the system reuses IDs.
function parentOf(process: ProcessInfo, byPid: Map<number, ProcessInfo>) {
  if (process.parent_pid === undefined || process.parent_pid === process.pid) return undefined;
  const parent = byPid.get(process.parent_pid);
  if (!parent) return undefined;
  if (parent.started_at !== undefined && process.started_at !== undefined && parent.started_at > process.started_at) return undefined;
  return parent;
}

export type TreeRow = { process: ProcessInfo; depth: number; children: number };

// The processes as a tree by parent, each level in `sort` order. A process
// whose parent isn't listed is a root. With a query, a match shows with
// its ancestors, so it stays in its place in the tree.
export function processTree(processes: readonly ProcessInfo[], sort: Sort<ProcessSortKey>, query = "", collapsed: ReadonlySet<number> = new Set()): TreeRow[] {
  const byPid = new Map(processes.map((process) => [process.pid, process]));
  const children = new Map<number | undefined, ProcessInfo[]>();
  for (const process of processes) {
    const parent = parentOf(process, byPid)?.pid;
    const siblings = children.get(parent) ?? [];
    siblings.push(process);
    children.set(parent, siblings);
  }
  let shown: Set<number> | null = null;
  if (query.trim()) {
    shown = new Set();
    for (const process of processes) {
      if (!processMatches(process, query)) continue;
      const seen = new Set<number>();
      for (let current: ProcessInfo | undefined = process; current && !seen.has(current.pid); current = parentOf(current, byPid)) {
        seen.add(current.pid);
        shown.add(current.pid);
      }
    }
  }
  const rows: TreeRow[] = [];
  const visited = new Set<number>();
  const walk = (parent: number | undefined, depth: number) => {
    for (const process of sortProcesses(children.get(parent) ?? [], sort)) {
      if (visited.has(process.pid) || (shown && !shown.has(process.pid))) continue;
      visited.add(process.pid);
      const kids = (children.get(process.pid) ?? []).filter((child) => !shown || shown.has(child.pid));
      rows.push({ process, depth, children: kids.length });
      if (!collapsed.has(process.pid) || shown) walk(process.pid, depth + 1);
    }
  };
  walk(undefined, 0);
  return rows;
}

// `process` and everything it started, children before their parents, in
// the order the device ends them.
export function processTreeOf(processes: readonly ProcessInfo[], pid: number): ProcessInfo[] {
  const byPid = new Map(processes.map((process) => [process.pid, process]));
  const root = byPid.get(pid);
  if (!root) return [];
  const result: ProcessInfo[] = [];
  const seen = new Set<number>();
  const walk = (process: ProcessInfo) => {
    if (seen.has(process.pid)) return;
    seen.add(process.pid);
    for (const child of processes) {
      if (child.pid !== process.pid && parentOf(child, byPid)?.pid === process.pid) walk(child);
    }
    result.push(process);
  };
  walk(root);
  return result;
}

export const formatCpu = (percent: number) => (percent < 0.05 ? "0%" : percent < 10 ? `${percent.toFixed(1)}%` : `${Math.round(percent)}%`);

export type ServiceState = "running" | "stopped" | "starting" | "stopping" | "paused" | "failed" | "other";
export type ServiceStartup = "automatic" | "automatic_delayed" | "manual" | "disabled" | "static";
export type ServiceAction = "start" | "stop" | "restart";

export type ServiceInfo = {
  name: string;
  display_name: string;
  state: ServiceState;
  state_detail?: string;
  startup?: ServiceStartup;
  startup_detail?: string;
  startup_choices?: ServiceStartup[];
  account?: string;
  description?: string;
  pid?: number;
  protected?: boolean;
};

export type ServiceList = { services: ServiceInfo[]; read_at: number; truncated?: boolean };

export const STATE_LABELS: Record<ServiceState, string> = {
  running: "Running",
  stopped: "Stopped",
  starting: "Starting",
  stopping: "Stopping",
  paused: "Paused",
  failed: "Failed",
  other: "Other",
};

// How a service starts, in its platform's words.
export function startupLabel(startup: ServiceStartup, platform: DevicePlatform) {
  if (platform === "macos") return startup === "disabled" ? "Disabled" : "Enabled";
  if (platform === "linux") {
    return { automatic: "Enabled (at boot)", automatic_delayed: "Enabled (at boot)", manual: "Disabled (on demand)", disabled: "Masked", static: "Static" }[startup];
  }
  return { automatic: "Automatic", automatic_delayed: "Automatic (Delayed Start)", manual: "Manual", disabled: "Disabled", static: "Boot or system" }[startup];
}

// What each startup choice does, for the dialog that changes it.
export function startupHelp(startup: ServiceStartup, platform: DevicePlatform) {
  if (platform === "macos") return startup === "disabled" ? "launchd won't start it, even when asked (launchctl disable)." : "launchd starts it as its job file says (launchctl enable).";
  if (platform === "linux") {
    return {
      automatic: "Starts at boot (systemctl enable).",
      automatic_delayed: "",
      manual: "Starts only when something asks for it (systemctl disable).",
      disabled: "Can't be started at all, even by other units (systemctl mask).",
      static: "",
    }[startup];
  }
  return {
    automatic: "Starts with Windows.",
    automatic_delayed: "Starts shortly after Windows has started the others.",
    manual: "Starts when a program or another service asks for it.",
    disabled: "Can't be started.",
    static: "",
  }[startup];
}

export type ServiceFilter = "all" | "running" | "stopped";

export type ServiceSortKey = "name" | "state" | "startup" | "account";
export const DEFAULT_SERVICE_SORT: Sort<ServiceSortKey> = { key: "name", descending: false };

const STATE_ORDER: ServiceState[] = ["running", "starting", "stopping", "paused", "failed", "stopped", "other"];
const STARTUP_ORDER: ServiceStartup[] = ["automatic", "automatic_delayed", "manual", "static", "disabled"];

export function visibleServices(services: readonly ServiceInfo[], filter: ServiceFilter, query: string, sort: Sort<ServiceSortKey>) {
  const needle = query.trim().toLowerCase();
  const shown = services.filter((service) => {
    if (filter === "running" && service.state !== "running") return false;
    if (filter === "stopped" && service.state === "running") return false;
    if (!needle) return true;
    return [service.name, service.display_name, service.description, service.account].some((field) => field?.toLowerCase().includes(needle));
  });
  const sign = sort.descending ? -1 : 1;
  const byName = (a: ServiceInfo, b: ServiceInfo) => collator.compare(a.display_name, b.display_name) || collator.compare(a.name, b.name);
  return shown.sort((a, b) => {
    switch (sort.key) {
      case "name":
        return sign * byName(a, b);
      case "state":
        return sign * (STATE_ORDER.indexOf(a.state) - STATE_ORDER.indexOf(b.state)) || byName(a, b);
      case "startup":
        return sign * (STARTUP_ORDER.indexOf(a.startup ?? "static") - STARTUP_ORDER.indexOf(b.startup ?? "static")) || byName(a, b);
      case "account":
        return sign * collator.compare(a.account ?? "", b.account ?? "") || byName(a, b);
    }
  });
}

// The actions a service offers now: start a stopped one, stop or restart a
// running one, never stop the Agent's own.
export function serviceActions(service: ServiceInfo): ServiceAction[] {
  const running = service.state === "running" || service.state === "starting" || service.state === "paused";
  if (service.protected) return running ? [] : ["start"];
  if (running) return ["stop", "restart"];
  if (service.state === "stopping") return [];
  return service.startup === "disabled" ? [] : ["start"];
}

export const ACTION_LABELS: Record<ServiceAction, string> = { start: "Start", stop: "Stop", restart: "Restart" };

// What to tell the technician when a request failed with `code`.
export function errorHelp(code: string | undefined): string | null {
  switch (code) {
    case "device_offline":
      return "The device is offline. The tools work again when it reconnects.";
    case "device_timeout":
      return "The device didn’t answer in time. It may have done it anyway, so refresh in a moment to see.";
    case "device_busy":
    case "busy":
      return "The device is busy. Try again in a moment.";
    default:
      return null;
  }
}
