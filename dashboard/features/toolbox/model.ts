// The toolbox's data and the rules the server enforces, so forms can explain
// a problem before saving. See docs/toolbox.md.

export type ScriptLanguage = "powershell" | "cmd" | "shell";
export type RunAs = "user" | "system";

export type ToolboxScript = {
  id: string;
  name: string;
  /** `/`-separated folder names; empty at the top level. */
  folder: string;
  description: string;
  language: ScriptLanguage;
  timeout_seconds: number;
  /** Shared with the whole company, rather than private to its owner. */
  shared: boolean;
  /** The signed-in user added it. */
  owned: boolean;
  can_edit: boolean;
  created_at_unix_ms: number;
  updated_at_unix_ms: number;
  /** Only when the script is read on its own. */
  body?: string;
};

export type ToolboxFile = {
  id: string;
  name: string;
  folder: string;
  size_bytes: number;
  sha256: string;
  shared: boolean;
  owned: boolean;
  can_edit: boolean;
  created_at_unix_ms: number;
  updated_at_unix_ms: number;
};

export type Toolbox = { scripts: ToolboxScript[]; files: ToolboxFile[] };

export type ScriptRunStatus = "pending" | "completed" | "failed" | "timed_out" | "lost";

export type ScriptRun = {
  id: string;
  device_id: string;
  script_id: string;
  script_name: string;
  language: ScriptLanguage;
  run_as: RunAs;
  status: ScriptRunStatus;
  /** The account it ran as, such as `NT AUTHORITY\SYSTEM` or `PC\ada`. */
  ran_as?: string;
  exit_code?: number;
  /** Lists leave output out; a run read on its own has it. */
  stdout: string;
  stderr: string;
  output_truncated: boolean;
  error?: string;
  created_at_unix_ms: number;
  completed_at_unix_ms?: number;
  source: "dashboard" | "session";
  requested_by_you: boolean;
};

/** What the script editor holds. The timeout stays text while it is typed. */
export type ScriptDraft = {
  name: string;
  folder: string;
  description: string;
  language: ScriptLanguage;
  body: string;
  timeoutSeconds: string;
  shared: boolean;
};

export const MAX_SCRIPT_BODY_BYTES = 128 * 1024;
export const MAX_SCRIPT_NAME_CHARS = 120;
export const MAX_SCRIPT_DESCRIPTION_BYTES = 1000;
export const MIN_SCRIPT_TIMEOUT_SECONDS = 10;
export const MAX_SCRIPT_TIMEOUT_SECONDS = 3600;
export const DEFAULT_SCRIPT_TIMEOUT_SECONDS = 300;
export const MAX_FILE_NAME_CHARS = 255;
export const MAX_FOLDER_DEPTH = 8;
export const MAX_FOLDER_NAME_CHARS = 64;
export const MAX_FOLDER_BYTES = 255;
export const MAX_TOOLBOX_FILE_BYTES = 95 * 1024 * 1024;

export const LANGUAGE_LABELS: Record<ScriptLanguage, string> = {
  powershell: "PowerShell",
  cmd: "Command Prompt",
  shell: "Shell (zsh)",
};

export const RUN_AS_LABELS: Record<RunAs, string> = {
  user: "Signed-in user",
  system: "System account",
};

const encoder = new TextEncoder();
export const utf8Length = (text: string) => encoder.encode(text).length;

// Unicode control characters, which names may not contain.
// eslint-disable-next-line no-control-regex
const CONTROL = /[\u0000-\u001f\u007f-\u009f]/u;
const characters = (text: string) => Array.from(text).length;

export const emptyScriptDraft = (folder = ""): ScriptDraft => ({
  name: "",
  folder,
  description: "",
  language: "powershell",
  body: "",
  timeoutSeconds: String(DEFAULT_SCRIPT_TIMEOUT_SECONDS),
  shared: false,
});

export const scriptDraft = (script: ToolboxScript): ScriptDraft => ({
  name: script.name,
  folder: script.folder,
  description: script.description,
  language: script.language,
  body: script.body ?? "",
  timeoutSeconds: String(script.timeout_seconds),
  shared: script.shared,
});

/**
 * A folder path as the server stores it: names trimmed, empty names dropped,
 * joined by `/`. `null` when it is too deep or a name is too long.
 */
export function normalizeFolder(folder: string): string | null {
  const names = folder.split(/[/\\]/u).map((name) => name.trim()).filter(Boolean);
  if (names.length > MAX_FOLDER_DEPTH) return null;
  if (names.some((name) => characters(name) > MAX_FOLDER_NAME_CHARS || CONTROL.test(name))) return null;
  const normalized = names.join("/");
  return utf8Length(normalized) <= MAX_FOLDER_BYTES ? normalized : null;
}

const RESERVED_FILE_NAMES = new Set([
  "CON", "PRN", "AUX", "NUL",
  ...Array.from({ length: 9 }, (_, index) => `COM${index + 1}`),
  ...Array.from({ length: 9 }, (_, index) => `LPT${index + 1}`),
]);

/** A file name Windows can create: no forbidden characters, no trailing dot or space, not a device name. */
export function isValidFileName(name: string) {
  if (!name || characters(name) > MAX_FILE_NAME_CHARS) return false;
  if (/[<>:"/\\|?*]/u.test(name) || CONTROL.test(name)) return false;
  if (/[. ]$/u.test(name) || name.startsWith(" ")) return false;
  const stem = name.split(".")[0].trimEnd().toUpperCase();
  return !RESERVED_FILE_NAMES.has(stem);
}

/** Why the draft cannot be saved, or `null` when it can. */
export function scriptDraftProblem(draft: ScriptDraft): string | null {
  const name = draft.name.trim();
  if (!name || characters(name) > MAX_SCRIPT_NAME_CHARS || CONTROL.test(name)) {
    return `Give the script a name of up to ${MAX_SCRIPT_NAME_CHARS} characters on one line.`;
  }
  if (normalizeFolder(draft.folder) === null) {
    return `Folders can be up to ${MAX_FOLDER_DEPTH} levels deep, with names of up to ${MAX_FOLDER_NAME_CHARS} characters.`;
  }
  if (utf8Length(draft.description.trim()) > MAX_SCRIPT_DESCRIPTION_BYTES) {
    return `Shorten the description to ${MAX_SCRIPT_DESCRIPTION_BYTES} bytes.`;
  }
  if (!draft.body.trim()) return "The script is empty.";
  if (utf8Length(draft.body) > MAX_SCRIPT_BODY_BYTES) return "Scripts can be up to 128 KiB.";
  if (draft.body.includes("\u0000")) return "Remove the NUL character from the script.";
  const timeout = Number(draft.timeoutSeconds);
  if (!Number.isInteger(timeout) || timeout < MIN_SCRIPT_TIMEOUT_SECONDS || timeout > MAX_SCRIPT_TIMEOUT_SECONDS) {
    return `The timeout must be a whole number of seconds from ${MIN_SCRIPT_TIMEOUT_SECONDS} to ${MAX_SCRIPT_TIMEOUT_SECONDS}.`;
  }
  return null;
}

/** The request body that saves the draft. Call only when it has no problem. */
export const scriptBody = (draft: ScriptDraft) => ({
  name: draft.name.trim(),
  folder: normalizeFolder(draft.folder) ?? "",
  description: draft.description.trim(),
  language: draft.language,
  body: draft.body,
  timeout_seconds: Number(draft.timeoutSeconds),
  shared: draft.shared,
});

/** Why a file cannot be uploaded, or `null` when it can. */
export function uploadProblem(file: { name: string; size: number }): string | null {
  if (!isValidFileName(file.name)) return "Windows can't use this file name.";
  if (file.size > MAX_TOOLBOX_FILE_BYTES) return "Files can be up to 95 MiB.";
  return null;
}

const collator = new Intl.Collator(undefined, { sensitivity: "base", numeric: true });

/** Items grouped by folder, top level first, then folders and names in order. */
export function groupByFolder<T extends { folder: string; name: string }>(items: readonly T[]) {
  const groups = new Map<string, T[]>();
  for (const item of items) {
    const group = groups.get(item.folder);
    if (group) group.push(item);
    else groups.set(item.folder, [item]);
  }
  return [...groups.entries()]
    .sort(([left], [right]) => (left === "" ? -1 : right === "" ? 1 : collator.compare(left, right)))
    .map(([folder, grouped]) => ({ folder, items: [...grouped].sort((left, right) => collator.compare(left.name, right.name)) }));
}

/** Every folder the items are in, and the folders above them, for suggestions. */
export function folderSuggestions(items: readonly { folder: string }[]) {
  const folders = new Set<string>();
  for (const { folder } of items) {
    const names = folder.split("/").filter(Boolean);
    names.forEach((_, index) => folders.add(names.slice(0, index + 1).join("/")));
  }
  return [...folders].sort(collator.compare);
}

/** Whether an item matches the search box: by name, folder or description. */
export function matchesQuery(item: { name: string; folder: string; description?: string }, query: string) {
  const needle = query.trim().toLocaleLowerCase();
  if (!needle) return true;
  return [item.name, item.folder, item.description ?? ""].some((value) => value.toLocaleLowerCase().includes(needle));
}

export function formatBytes(bytes: number) {
  if (bytes < 1024) return `${bytes} B`;
  const units = ["KiB", "MiB", "GiB"];
  let value = bytes / 1024;
  let unit = 0;
  while (value >= 1024 && unit < units.length - 1) {
    value /= 1024;
    unit += 1;
  }
  return `${value >= 10 ? Math.round(value) : value.toFixed(1)} ${units[unit]}`;
}

export const isRunFinished = (run: Pick<ScriptRun, "status">) => run.status !== "pending";

/** A run's outcome in a few words. */
export function runOutcome(run: Pick<ScriptRun, "status" | "exit_code">) {
  switch (run.status) {
    case "pending": return "Running…";
    case "completed": return run.exit_code === undefined ? "Finished" : `Exit code ${run.exit_code}`;
    case "failed": return "Couldn't run";
    case "timed_out": return "Timed out";
    case "lost": return "No result";
  }
}

/** How a run's outcome is colored: still going, it worked, or it didn't. */
export function runTone(run: Pick<ScriptRun, "status" | "exit_code">): "pending" | "success" | "problem" {
  if (run.status === "pending") return "pending";
  return run.status === "completed" && run.exit_code === 0 ? "success" : "problem";
}

/** SYSTEM on Windows, root on a Mac. */
const SYSTEM_ACCOUNT = /(?:\\SYSTEM|^root)$/iu;

/** The account a run used, noting when nobody was signed in to run it as. */
export function ranAsLabel(run: Pick<ScriptRun, "run_as" | "ran_as">) {
  if (!run.ran_as) return RUN_AS_LABELS[run.run_as];
  if (run.run_as === "user" && SYSTEM_ACCOUNT.test(run.ran_as)) return `${run.ran_as} (nobody was signed in)`;
  return run.ran_as;
}

/** Why a run has no result, for runs that are lost. */
export const LOST_RUN_EXPLANATION = "The device didn't report a result. It may have gone offline or restarted while the script ran.";
