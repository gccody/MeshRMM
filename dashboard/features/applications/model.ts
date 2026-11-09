// Third-party applications as the server sends them: the library, each
// device's packages, the fleet's outdated ones, and installs, updates and
// uninstalls. How the website words, checks and filters them. See
// docs/application-deployment.md. Imports carry the .ts extension so
// node:test can load this module directly.
import type { DevicePlatform } from "../agents/types.ts";

export type PackageSource = "winget" | "chocolatey" | "homebrew" | "homebrew_cask" | "flatpak" | "snap" | "custom";
export type ManagerSource = Exclude<PackageSource, "custom">;

export const SOURCES: readonly PackageSource[] = ["winget", "chocolatey", "homebrew", "homebrew_cask", "flatpak", "snap", "custom"];

export const SOURCE_LABELS: Record<PackageSource, string> = {
  winget: "winget",
  chocolatey: "Chocolatey",
  homebrew: "Homebrew",
  homebrew_cask: "Homebrew cask",
  flatpak: "Flatpak",
  snap: "Snap",
  custom: "Custom installer",
};

// The platform each package manager runs on.
export const SOURCE_PLATFORM: Record<ManagerSource, DevicePlatform> = {
  winget: "windows",
  chocolatey: "windows",
  homebrew: "macos",
  homebrew_cask: "macos",
  flatpak: "linux",
  snap: "linux",
};

// What a package ID looks like, for the editor's placeholder.
export const SOURCE_EXAMPLES: Record<ManagerSource, string> = {
  winget: "Mozilla.Firefox",
  chocolatey: "firefox",
  homebrew: "wget",
  homebrew_cask: "firefox",
  flatpak: "org.mozilla.firefox",
  snap: "firefox",
};

// The installer files each platform runs.
export const INSTALLER_KINDS: Record<DevicePlatform, string> = {
  windows: ".msi, .exe, .ps1, .cmd or .bat",
  macos: ".pkg or .sh",
  linux: ".deb, .rpm, .sh, .run or .bin",
};

export type DetectionRule =
  | { kind: "application"; name: string; min_version?: string }
  | { kind: "file"; path: string }
  | { kind: "registry"; key: string; value?: string };

// One way to install a library application.
export type Source = {
  source: PackageSource;
  platform: DevicePlatform;
  // Empty for a custom installer.
  package_id: string;
  file_id?: string;
  // The installer file, while it is in the toolbox.
  file_name?: string;
  file_size_bytes?: number;
  file_sha256?: string;
  install_arguments?: string;
  uninstall_command?: string;
  detection?: DetectionRule;
};

// GET /v1/applications
export type Application = {
  id: string;
  name: string;
  description: string;
  pinned_version: string | null;
  sources: Source[];
  created_at: number;
  updated_at: number;
  // How many devices have it, and have it outdated.
  installed: number;
  outdated: number;
  // The policies that install or update it, by name.
  policies: string[];
};

export type ManagerState = { source: PackageSource; version?: string; error?: string };

// A device's package managers at its last scan.
export type ScanStatus = { managers: ManagerState[]; scanned_at: number; received_at: number };

// A package installed on a device.
export type DevicePackage = {
  source: PackageSource;
  package_id: string;
  name: string;
  version: string | null;
  available_version: string | null;
  // When the server first saw it outdated.
  outdated_since: number | null;
  // The library application it is, if any.
  application_id: string | null;
  application_name: string | null;
  // Pinned in the library: updates leave it alone.
  held: boolean;
};

export type RunAction = "scan" | "install" | "update" | "uninstall";
export type RunStatus = "pending" | "succeeded" | "partial" | "failed" | "lost";
export type ItemOutcome = "succeeded" | "unchanged" | "failed";
export type UpdateMode = "all" | "only" | "except";

export type RunItem = {
  position: number;
  name: string;
  source: PackageSource | null;
  package_id: string | null;
  outcome: ItemOutcome;
  version: string | null;
  exit_code: number | null;
  error: string | null;
  // A single run's only; lists leave it out.
  output?: string;
  reboot_required: boolean;
};

// A scan, install, update or uninstall the server asked a device for.
export type ApplicationRun = {
  id: string;
  device_id: string;
  device_name: string | null;
  action: RunAction;
  // Such as "Install Firefox and 7-Zip".
  summary: string;
  status: RunStatus;
  progress: string | null;
  requested_by: string;
  policy_name: string | null;
  job_id: string | null;
  error: string | null;
  succeeded: number;
  failed: number;
  reboot_required: boolean;
  created_at: number;
  completed_at: number | null;
  items?: RunItem[];
};

// GET /v1/agents/{id}/packages
export type DevicePackages = {
  device_id: string;
  // Null before the device's first scan.
  status: ScanStatus | null;
  scanning: ApplicationRun | null;
  running: ApplicationRun | null;
  packages: DevicePackage[];
};

export type OutdatedDevice = {
  device_id: string;
  device_name: string;
  version: string | null;
  available_version: string | null;
  outdated_since: number | null;
};

// GET /v1/packages/outdated: one package and the devices it is outdated on.
export type OutdatedPackage = {
  source: PackageSource;
  package_id: string;
  name: string;
  application_id: string | null;
  application_name: string | null;
  held: boolean;
  devices: OutdatedDevice[];
};

// A Windows program from the device's inventory, to uninstall.
export type InventoryProgram = { name: string; version?: string };

// POST /v1/agents/{id}/application-runs, and a job's `applications`.
export type RunRequest = {
  action: Exclude<RunAction, "scan">;
  application_ids?: string[];
  packages?: string[];
  mode?: UpdateMode;
  inventory?: InventoryProgram[];
};

export const ACTION_LABELS: Record<RunAction, string> = {
  scan: "Scan",
  install: "Install",
  update: "Update",
  uninstall: "Uninstall",
};

export const RUN_STATUS_LABELS: Record<RunStatus, string> = {
  pending: "Running",
  succeeded: "Succeeded",
  partial: "Partly failed",
  failed: "Failed",
  lost: "No result",
};

export const OUTCOME_LABELS: Record<ItemOutcome, string> = {
  succeeded: "Done",
  unchanged: "Nothing to do",
  failed: "Failed",
};

export const MODE_LABELS: Record<UpdateMode, string> = {
  all: "Every outdated application",
  only: "Only the chosen applications",
  except: "Every outdated application except the chosen ones",
};

export const runTone = (status: RunStatus) =>
  status === "succeeded" ? "success" : status === "pending" ? "pending" : "problem";

export const MAX_RUN_ITEMS = 50;
export const MAX_NAME_CHARS = 120;
export const MAX_DESCRIPTION_BYTES = 1000;
export const MAX_SOURCES = 12;
export const MAX_COMMAND_CHARS = 2000;
const MAX_PACKAGE_ID_CHARS = 200;
const MAX_VERSION_CHARS = 64;

// A package as the API names it: `source:id`.
export const packageKey = (pkg: { source: PackageSource; package_id: string }) => `${pkg.source}:${pkg.package_id}`;

const ID_EXTRA: Record<ManagerSource, string> = {
  winget: "._-+",
  chocolatey: "._-",
  homebrew: "._-+@/",
  homebrew_cask: "._-+@/",
  flatpak: "._-",
  snap: "-",
};

// Whether `id` could be a package ID of the manager, as the server checks:
// letters, digits and the manager's punctuation, starting with a letter or
// digit, so it can never be read as an option.
export function validPackageId(source: ManagerSource, id: string) {
  if (!id || Array.from(id).length > MAX_PACKAGE_ID_CHARS || id.includes("..")) return false;
  if (!/^[A-Za-z0-9]/u.test(id)) return false;
  if (source === "snap" && /[A-Z]/u.test(id)) return false;
  const extra = ID_EXTRA[source];
  return Array.from(id).every((char) => /^[A-Za-z0-9]$/u.test(char) || extra.includes(char));
}

export const validVersion = (version: string) =>
  version.length > 0 && version.length <= MAX_VERSION_CHARS && /^[A-Za-z0-9][A-Za-z0-9.\-_+~:]*$/u.test(version);

// eslint-disable-next-line no-control-regex
const CONTROL = /[\u0000-\u001f\u007f-\u009f]/u;
const REGISTRY_ROOTS = ["HKLM", "HKEY_LOCAL_MACHINE", "HKCU", "HKEY_CURRENT_USER", "HKU", "HKEY_USERS", "HKCR", "HKEY_CLASSES_ROOT"];

// The platforms a library application installs on.
export const platformsOf = (application: Pick<Application, "sources">): DevicePlatform[] =>
  (["windows", "macos", "linux"] as const).filter((platform) => application.sources.some((source) => source.platform === platform));

// "winget Mozilla.Firefox", or the installer's file name.
export function describeSource(source: Source) {
  if (source.source === "custom") return `${source.file_name ?? "Deleted file"}`;
  return `${SOURCE_LABELS[source.source]} ${source.package_id}`;
}

// The library applications that install on `platform`.
export const applicationsFor = (applications: readonly Application[], platform: DevicePlatform) =>
  applications.filter((application) => application.sources.some((source) => source.platform === platform));

// "Firefox", "Firefox and 7-Zip", or "3 applications", as the server words
// summaries.
export function namesText(names: readonly string[]) {
  if (names.length === 0) return "nothing";
  if (names.length === 1) return names[0];
  if (names.length === 2) return `${names[0]} and ${names[1]}`;
  return `${names.length} applications`;
}

// What an update covers, in words.
export function describeUpdate(mode: UpdateMode, names: readonly string[]) {
  if (mode === "all") return "Update all applications";
  if (mode === "only") return `Update ${namesText(names)}`;
  return `Update all applications except ${namesText(names)}`;
}

// "Firefox 128.0 → 129.0", for an outdated package.
export function versionChange(pkg: Pick<DevicePackage, "version" | "available_version">) {
  if (!pkg.available_version) return pkg.version ?? "";
  return `${pkg.version ?? "?"} → ${pkg.available_version}`;
}

// The device's packages that a search matches, by name, ID or library
// application, outdated first as the server sends them.
export function filterPackages(packages: readonly DevicePackage[], query: string, outdatedOnly: boolean) {
  const needle = query.trim().toLocaleLowerCase();
  return packages.filter((pkg) =>
    (!outdatedOnly || pkg.available_version !== null)
    && (!needle || [pkg.name, pkg.package_id, pkg.application_name ?? ""].some((text) => text.toLocaleLowerCase().includes(needle))));
}

// Fleet totals for the outdated view's heading.
export function outdatedTotals(packages: readonly OutdatedPackage[]) {
  const devices = new Set<string>();
  let updates = 0;
  for (const pkg of packages) {
    for (const device of pkg.devices) devices.add(device.device_id);
    updates += pkg.devices.length;
  }
  return { packages: packages.length, devices: devices.size, updates };
}

// The oldest time a package became outdated on any device.
export const oldestOutdated = (pkg: OutdatedPackage) =>
  pkg.devices.reduce<number | null>((oldest, device) =>
    device.outdated_since !== null && (oldest === null || device.outdated_since < oldest) ? device.outdated_since : oldest, null);

// Windows programs from the inventory that MeshRMM can uninstall: machine
// wide ones (per-user programs need their user signed in).
export const uninstallableInventory = <T extends { source: string }>(programs: readonly T[]) =>
  programs.filter((program) => program.source === "windows");

// ---- The library editor ----

export type DetectionKind = DetectionRule["kind"];

export type SourceDraft = {
  source: PackageSource;
  // A custom installer's platform; package managers have their own.
  platform: DevicePlatform;
  packageId: string;
  fileId: string;
  installArguments: string;
  uninstallCommand: string;
  detection: DetectionKind;
  detectionName: string;
  detectionMinVersion: string;
  detectionPath: string;
  detectionKey: string;
  detectionValue: string;
};

export type ApplicationDraft = {
  name: string;
  description: string;
  pinnedVersion: string;
  sources: SourceDraft[];
};

export function sourceDraft(source: Partial<Source> & Pick<Source, "source">): SourceDraft {
  const detection = source.detection;
  const platform = source.source === "custom" ? source.platform ?? "windows" : SOURCE_PLATFORM[source.source];
  return {
    source: source.source,
    platform,
    packageId: source.package_id ?? "",
    fileId: source.file_id ?? "",
    installArguments: source.install_arguments ?? "",
    uninstallCommand: source.uninstall_command ?? "",
    detection: detection?.kind ?? "application",
    detectionName: detection?.kind === "application" ? detection.name : "",
    detectionMinVersion: detection?.kind === "application" ? detection.min_version ?? "" : "",
    detectionPath: detection?.kind === "file" ? detection.path : "",
    detectionKey: detection?.kind === "registry" ? detection.key : "",
    detectionValue: detection?.kind === "registry" ? detection.value ?? "" : "",
  };
}

export function applicationDraft(application: Application | null): ApplicationDraft {
  return {
    name: application?.name ?? "",
    description: application?.description ?? "",
    pinnedVersion: application?.pinned_version ?? "",
    sources: application ? application.sources.map(sourceDraft) : [sourceDraft({ source: "winget" })],
  };
}

const oneLine = (text: string) => Array.from(text).length <= MAX_COMMAND_CHARS && !CONTROL.test(text);

function sourceProblem(draft: SourceDraft): string | null {
  if (draft.source !== "custom") {
    const id = draft.packageId.trim();
    if (!id) return `Enter the ${SOURCE_LABELS[draft.source]} package ID.`;
    if (!validPackageId(draft.source, id)) return `“${id}” isn’t a ${SOURCE_LABELS[draft.source]} package ID.`;
    return null;
  }
  if (!draft.fileId) return "Choose the installer file from the toolbox.";
  if (!oneLine(draft.installArguments.trim())) return `Installer arguments must be at most ${MAX_COMMAND_CHARS} characters on one line.`;
  if (!oneLine(draft.uninstallCommand.trim())) return `The uninstall command must be at most ${MAX_COMMAND_CHARS} characters on one line.`;
  switch (draft.detection) {
    case "application": {
      const name = draft.detectionName.trim();
      if (!name || !oneLine(name)) return "Enter the name the installed application has in the device’s software list.";
      const version = draft.detectionMinVersion.trim();
      if (version && !validVersion(version)) return "The minimum version may use letters, digits and . - _ + ~ : only.";
      return null;
    }
    case "file": {
      const path = draft.detectionPath.trim();
      return path && oneLine(path) ? null : "Enter the path of a file or folder the installer creates.";
    }
    case "registry": {
      if (draft.platform !== "windows") return "Registry detection is for Windows installers.";
      const key = draft.detectionKey.trim();
      const root = key.split("\\")[0].toUpperCase();
      if (!key || !oneLine(key) || !REGISTRY_ROOTS.includes(root)) return "Enter a registry key starting with HKLM, HKCU, HKU or HKCR.";
      return oneLine(draft.detectionValue.trim()) ? null : "The registry value name isn’t valid.";
    }
  }
}

// Why the draft can't be saved, or null when it can.
export function applicationDraftProblem(draft: ApplicationDraft): string | null {
  const name = draft.name.trim();
  if (!name || Array.from(name).length > MAX_NAME_CHARS || CONTROL.test(name)) return `Give the application a name of up to ${MAX_NAME_CHARS} characters.`;
  if (new TextEncoder().encode(draft.description.trim()).length > MAX_DESCRIPTION_BYTES) return `The description must be at most ${MAX_DESCRIPTION_BYTES} bytes.`;
  const version = draft.pinnedVersion.trim();
  if (version && !validVersion(version)) return "The pinned version may use letters, digits and . - _ + ~ : only.";
  if (!draft.sources.length) return "Add at least one way to install the application.";
  if (draft.sources.length > MAX_SOURCES) return `An application has at most ${MAX_SOURCES} ways to install.`;
  for (const source of draft.sources) {
    const problem = sourceProblem(source);
    if (problem) return problem;
  }
  const seen = new Set<string>();
  for (const source of draft.sources) {
    const key = source.source === "custom" ? `custom:${source.platform}:${source.fileId}` : `${source.source}:${source.packageId.trim().toLowerCase()}`;
    if (seen.has(key)) return "The application lists the same source twice.";
    seen.add(key);
  }
  return null;
}

const blank = (text: string) => text.trim() || undefined;

function sourceInput(draft: SourceDraft) {
  if (draft.source !== "custom") return { source: draft.source, package_id: draft.packageId.trim() };
  const detection: DetectionRule = draft.detection === "application"
    ? { kind: "application", name: draft.detectionName.trim(), ...(blank(draft.detectionMinVersion) && { min_version: draft.detectionMinVersion.trim() }) }
    : draft.detection === "file"
      ? { kind: "file", path: draft.detectionPath.trim() }
      : { kind: "registry", key: draft.detectionKey.trim(), ...(blank(draft.detectionValue) && { value: draft.detectionValue.trim() }) };
  return {
    source: "custom",
    platform: draft.platform,
    file_id: draft.fileId,
    install_arguments: blank(draft.installArguments) ?? null,
    uninstall_command: blank(draft.uninstallCommand) ?? null,
    detection,
  };
}

const installers = (sources: readonly SourceDraft[]) =>
  sources.filter((source) => source.source === "custom").map((source) => JSON.stringify(sourceInput(source)));

// Whether saving the draft over `application` (null for a new one) adds or
// changes a custom installer, as the server sees it: that needs scripts.run
// as well as applications.manage, since custom installers run as SYSTEM or
// root. Removing or reordering ways to install doesn't.
export function installersChanged(application: Application | null, draft: ApplicationDraft): boolean {
  const before = new Set(installers(application?.sources.map(sourceDraft) ?? []));
  return installers(draft.sources).some((installer) => !before.has(installer));
}

export const INSTALLERS_NEED_SCRIPTS = "Adding or changing a custom installer needs the scripts.run permission too, because it runs as SYSTEM or root.";

// The request body that saves the draft. Call only when it has no problem.
export function applicationInput(draft: ApplicationDraft) {
  return {
    name: draft.name.trim(),
    description: draft.description.trim(),
    pinned_version: blank(draft.pinnedVersion) ?? null,
    sources: draft.sources.map(sourceInput),
  };
}
