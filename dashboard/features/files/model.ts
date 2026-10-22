// The file browser: what the server sends, paths on each platform, and the
// wording and arithmetic the page needs. Imports carry the .ts extension so
// node:test can load this module directly.
import type { DevicePlatform } from "../agents/types.ts";
import type { Permission } from "../auth/types.ts";

export type FileAccount = "system" | "user";
export type FileKind = "file" | "directory" | "other";
export type FileSort = "name" | "size" | "modified" | "type";

export type FileEntry = {
  name: string;
  kind: FileKind;
  link?: boolean;
  size?: number;
  modified?: number;
  hidden?: boolean;
  system?: boolean;
  read_only?: boolean;
};

export type FileListing = {
  path: string;
  parent?: string;
  account: string;
  offset: number;
  total: number;
  entries: FileEntry[];
  read_at: number;
  truncated?: boolean;
};

export type FileRoot = {
  path: string;
  label: string;
  kind: "root" | "fixed" | "removable" | "network" | "optical" | "other";
  total_bytes?: number;
  free_bytes?: number;
};

export type FilePlace = {
  path: string;
  kind: "home" | "desktop" | "documents" | "downloads";
  user: string;
};

export type FileRoots = { account: string; roots: FileRoot[]; places: FilePlace[] };

export type FileFailure = { path: string; error: { code: string; message: string } };
export type FileBatch = { done: string[]; failed: FileFailure[] };

export type DownloadProgress = {
  state: "ready" | "sending" | "done" | "failed";
  bytes: number;
  offset: number;
  size?: number;
  sha256?: string;
  error?: string;
};

export type DownloadView = {
  id: string;
  device_id: string;
  name: string;
  kind: "file" | "archive";
  size: number;
  files: number;
  modified?: number;
  url: string;
  progress: DownloadProgress;
};

export type UploadView = { id: string; device_id: string; path: string; size: number; received: number };

// Entries a listing page asks for.
export const PAGE_SIZE = 500;
// The height of a listing row, in pixels; globals.css matches it.
export const ROW_HEIGHT = 36;

// The accounts the user may browse as, the device's own first.
export function accountOptions(can: (permission: Permission) => boolean): FileAccount[] {
  if (!can("files.browse")) return [];
  return can("files.system") ? ["system", "user"] : ["user"];
}

export const accountLabel = (account: FileAccount, platform: DevicePlatform) =>
  account === "user" ? "Signed-in user" : platform === "windows" ? "SYSTEM" : "root";

export const trashName = (platform: DevicePlatform) => (platform === "windows" ? "Recycle Bin" : "Trash");

const windows = (platform: DevicePlatform) => platform === "windows";

export const separator = (platform: DevicePlatform) => (windows(platform) ? "\\" : "/");

// A Windows path's root: `C:\` or `\\server\share`.
function windowsRoot(path: string): string | null {
  const drive = /^[a-zA-Z]:\\?/u.exec(path);
  if (drive) return `${path.slice(0, 2)}\\`;
  const share = /^\\\\[^\\]+\\[^\\]+\\?/u.exec(path);
  return share ? share[0].replace(/\\$/u, "") : null;
}

// Whether `path` is a drive, a share or `/`.
export function isRoot(platform: DevicePlatform, path: string) {
  if (!windows(platform)) return path === "/";
  const root = windowsRoot(path);
  return root !== null && (path === root || `${path}\\` === root || path === `${root}\\`);
}

// `name` inside `folder`.
export function joinPath(platform: DevicePlatform, folder: string, name: string) {
  const sep = separator(platform);
  return folder.endsWith(sep) ? `${folder}${name}` : `${folder}${sep}${name}`;
}

// `parts`, separated by `/` as browsers name folder uploads, inside
// `folder`.
export function joinRelative(platform: DevicePlatform, folder: string, relative: string) {
  return relative
    .split("/")
    .filter((part) => part !== "" && part !== "." && part !== "..")
    .reduce((path, part) => joinPath(platform, path, part), folder);
}

// The folder above `path`, or null at a root.
export function parentPath(platform: DevicePlatform, path: string): string | null {
  if (isRoot(platform, path)) return null;
  const sep = separator(platform);
  const trimmed = path.endsWith(sep) ? path.slice(0, -1) : path;
  const cut = trimmed.lastIndexOf(sep);
  if (cut < 0) return null;
  if (!windows(platform)) return cut === 0 ? "/" : trimmed.slice(0, cut);
  const root = windowsRoot(trimmed);
  if (root && cut < root.length) return root;
  const parent = trimmed.slice(0, cut);
  return root && parent.length <= root.length ? root : parent;
}

// The last part of `path`.
export function baseName(platform: DevicePlatform, path: string) {
  if (isRoot(platform, path)) return path;
  const sep = separator(platform);
  const trimmed = path.endsWith(sep) ? path.slice(0, -1) : path;
  return trimmed.slice(trimmed.lastIndexOf(sep) + 1);
}

// The path's folders, from its root, each with where it leads.
export function crumbs(platform: DevicePlatform, path: string): { label: string; path: string }[] {
  const trail: { label: string; path: string }[] = [];
  for (let current: string | null = path; current !== null; current = parentPath(platform, current)) {
    trail.unshift({ label: isRoot(platform, current) ? current.replace(/\\$/u, "") || current : baseName(platform, current), path: current });
  }
  if (!windows(platform) && trail.length) trail[0].label = "/";
  return trail;
}

// What kind of entry it is, as people say it.
export function typeLabel(entry: FileEntry) {
  if (entry.kind === "directory") return entry.link ? "Link to folder" : "Folder";
  if (entry.kind === "other") return entry.link ? "Broken link" : "Special file";
  const dot = entry.name.lastIndexOf(".");
  const extension = dot > 0 ? entry.name.slice(dot + 1) : "";
  const kind = extension && extension.length <= 8 ? `${extension.toUpperCase()} file` : "File";
  return entry.link ? `Link to ${kind}` : kind;
}

// A rename's starting selection: the name without its extension.
export function stemLength(name: string, kind: FileKind) {
  const dot = name.lastIndexOf(".");
  return kind !== "directory" && dot > 0 ? dot : name.length;
}

// Whether `name` is one the device's platform allows for a new entry; the
// device has the last word.
export function nameProblem(platform: DevicePlatform, name: string): string | null {
  if (!name.trim()) return "Enter a name.";
  if (name === "." || name === "..") return "Enter a name.";
  if (name.includes("/") || (windows(platform) && name.includes("\\"))) return "A name can’t contain a slash.";
  if (windows(platform)) {
    // eslint-disable-next-line no-control-regex
    if (/[<>:"|?*\u0000-\u001f]/u.test(name)) return "Windows doesn’t allow < > : \" | ? * in names.";
    if (/[. ]$/u.test(name)) return "Windows doesn’t allow names ending in a dot or a space.";
    if (/^(con|prn|aux|nul|com[1-9]|lpt[1-9])(\..*)?$/iu.test(name.trim())) return "That name is reserved by Windows.";
  }
  return null;
}

// The rows to draw for a viewport: those visible plus `overscan` on each
// side, within `total`.
export function visibleRows(scrollTop: number, viewport: number, total: number, overscan = 10) {
  const first = Math.max(0, Math.floor(scrollTop / ROW_HEIGHT) - overscan);
  const last = Math.min(total, Math.ceil((scrollTop + viewport) / ROW_HEIGHT) + overscan);
  return { first, last: Math.max(first, last) };
}

// The listing pages rows `first` to `last` fall in.
export function pagesFor(first: number, last: number) {
  const pages: number[] = [];
  for (let page = Math.floor(first / PAGE_SIZE); page * PAGE_SIZE < Math.max(last, first + 1); page++) pages.push(page);
  return pages;
}

// What a failed request means for the folder shown, and the way out of it.
export function errorHelp(code: string | undefined, account: FileAccount, platform: DevicePlatform): string | null {
  switch (code) {
    case "access_denied":
      return account === "system"
        ? platform === "macos"
          ? "macOS may be keeping the Agent out. Give MeshRMM Agent Full Disk Access on the Mac, or try as the signed-in user."
          : null
        : "The signed-in user can’t open it. Try as " + accountLabel("system", platform) + ".";
    case "no_user":
      return `Nobody is signed in to the device. Browse as ${accountLabel("system", platform)} instead.`;
    case "device_offline":
    case "device_disconnected":
      return "The page carries on once the device is back online.";
    case "not_a_directory":
      return "Open its folder and download it from there.";
    default:
      return null;
  }
}

// Upload states the queue still works on.
export const ACTIVE_UPLOAD_STATES = ["hashing", "uploading", "verifying"] as const;

// `bytes` of `total`, as a percentage for a progress bar.
export const percent = (bytes: number, total: number) => (total > 0 ? Math.min(100, (bytes / total) * 100) : 100);

// A transfer's speed, from the bytes moved since it started.
export function rate(bytes: number, startedAt: number, now: number) {
  const seconds = (now - startedAt) / 1000;
  return seconds > 0.5 ? bytes / seconds : 0;
}

// The page's address for a device, a folder and an account.
export function filesPagePath(deviceId: string, path?: string, account?: FileAccount) {
  const params = new URLSearchParams({ id: deviceId });
  if (path) params.set("path", path);
  if (account) params.set("as", account);
  return `/files?${params}`;
}
