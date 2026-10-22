// Devices' hardware and software as the API returns them, and the sorting
// and searching the website does on them. Imports carry the .ts extension so
// node:test can load this module directly.

export type OperatingSystem = {
  id: string;
  id_like?: string[];
  name: string;
  version: string;
  build: string | null;
  kernel: string | null;
  architecture: string;
  installed_at_unix_ms: number | null;
  boot_time_unix_ms: number | null;
  time_zone: string | null;
  package_managers: string[];
};

export type PhysicalDisk = {
  name: string;
  model: string | null;
  size_bytes: number;
  media: "ssd" | "hdd" | null;
  interface: string | null;
};

export type Hardware = {
  manufacturer: string | null;
  model: string | null;
  serial_number: string | null;
  firmware_version: string | null;
  processor: { name: string; cores: number; threads: number } | null;
  memory_bytes: number;
  disks: PhysicalDisk[];
};

export type AdapterKind = "ethernet" | "wireless" | "virtual" | "other";

export type NetworkAdapter = {
  name: string;
  description: string | null;
  mac_address: string | null;
  kind: AdapterKind;
  up: boolean;
  speed_bits_per_second: number | null;
  addresses: { address: string; prefix_length: number }[];
  gateways: string[];
  dns_servers: string[];
};

export type Network = {
  hostname: string;
  fqdn: string | null;
  domain: string | null;
  workgroup: string | null;
  adapters: NetworkAdapter[];
  dns_servers: string[];
};

export type SignedInUser = {
  name: string;
  session: string | null;
  remote_host: string | null;
  since_unix_ms: number | null;
};

// `GET /v1/agents/{id}/system-info`.
export type Snapshot = {
  device_id: string;
  collected_at: number;
  received_at: number;
  public_ip: string | null;
  application_count: number;
  os: OperatingSystem;
  hardware: Hardware;
  network: Network;
  users: SignedInUser[];
  problems: string[];
};

export type ApplicationSource = "windows" | "windows_user" | "macos_app" | "dpkg" | "rpm" | "pacman" | "flatpak" | "snap";

export const SOURCE_LABELS: Record<ApplicationSource, string> = {
  windows: "Windows",
  windows_user: "Windows, one user",
  macos_app: "Application",
  dpkg: "dpkg",
  rpm: "RPM",
  pacman: "pacman",
  flatpak: "Flatpak",
  snap: "Snap",
};

export const sourceLabel = (source: string) => SOURCE_LABELS[source as ApplicationSource] ?? source;

export type Application = {
  name: string;
  version: string | null;
  publisher: string | null;
  architecture: string | null;
  installed_at: number | null;
  source: string;
};

export type ApplicationChange = {
  observed_at: number;
  change: "installed" | "removed" | "updated";
  name: string;
  version: string | null;
  previous_version: string | null;
  source: string;
};

// `GET /v1/software`.
export type FleetApplication = {
  name: string;
  publisher: string | null;
  devices: number;
  versions: { version: string | null; devices: number }[];
};

// `GET /v1/software/devices`.
export type InstalledOn = {
  device_id: string;
  device_name: string;
  platform: string;
  company_id: string | null;
  version: string | null;
  publisher: string | null;
  architecture: string | null;
  installed_at: number | null;
  source: string;
};

export type ApplicationSortKey = "name" | "version" | "publisher" | "installed_at" | "source";
export type SortDirection = "ascending" | "descending";
export type ApplicationSort = { key: ApplicationSortKey; direction: SortDirection };

export const DEFAULT_APPLICATION_SORT: ApplicationSort = { key: "name", direction: "ascending" };

const collator = new Intl.Collator(undefined, { numeric: true, sensitivity: "base" });

// Applications whose name, publisher or version contains `query`, ignoring
// case, in `sort` order. Missing values sort last either way.
export function visibleApplications(applications: readonly Application[], query: string, sort: ApplicationSort) {
  const search = query.trim().toLocaleLowerCase();
  const matches = search
    ? applications.filter((application) =>
        [application.name, application.publisher ?? "", application.version ?? ""].some((text) => text.toLocaleLowerCase().includes(search)))
    : [...applications];
  const sign = sort.direction === "ascending" ? 1 : -1;
  const value = (application: Application) =>
    sort.key === "source" ? sourceLabel(application.source) : application[sort.key];
  return matches.sort((left, right) => {
    const a = value(left);
    const b = value(right);
    if (a === null || a === undefined || a === "") return b === null || b === undefined || b === "" ? collator.compare(left.name, right.name) : 1;
    if (b === null || b === undefined || b === "") return -1;
    const order = typeof a === "number" && typeof b === "number" ? a - b : collator.compare(String(a), String(b));
    return sign * order || collator.compare(left.name, right.name);
  });
}

// The next sort after clicking a column's heading.
export function nextSort(current: ApplicationSort, key: ApplicationSortKey): ApplicationSort {
  if (current.key !== key) return { key, direction: key === "installed_at" ? "descending" : "ascending" };
  return { key, direction: current.direction === "ascending" ? "descending" : "ascending" };
}

// "Windows 11 Pro 24H2 (26100.4652)".
export function describeOs(os: OperatingSystem) {
  const name = [os.name, os.version].filter(Boolean).join(" ");
  return os.build ? `${name} (${os.build})` : name;
}

// The date part only, as dates of installs carry no meaningful time.
export function formatDate(unixMs: number | null | undefined) {
  if (unixMs === null || unixMs === undefined) return "—";
  return new Date(unixMs).toLocaleDateString([], { year: "numeric", month: "short", day: "numeric", timeZone: "UTC" });
}

export function formatLinkSpeed(bitsPerSecond: number | null) {
  if (!bitsPerSecond) return null;
  if (bitsPerSecond >= 1e9) return `${Number((bitsPerSecond / 1e9).toFixed(1))} Gbps`;
  if (bitsPerSecond >= 1e6) return `${Math.round(bitsPerSecond / 1e6)} Mbps`;
  return `${Math.round(bitsPerSecond / 1e3)} kbps`;
}

export const ADAPTER_KIND_LABELS: Record<AdapterKind, string> = {
  ethernet: "Ethernet",
  wireless: "Wi-Fi",
  virtual: "Virtual",
  other: "Other",
};

// Hardware adapters first, then by name; down adapters after up ones.
export function sortAdapters(adapters: readonly NetworkAdapter[]) {
  const rank = (adapter: NetworkAdapter) => (adapter.kind === "virtual" ? 2 : 0) + (adapter.up ? 0 : 1);
  return [...adapters].sort((left, right) => rank(left) - rank(right) || collator.compare(left.name, right.name));
}

export type SoftwareFilters = { query: string; company: string };

export const MAX_SOFTWARE_QUERY_LENGTH = 200;

export function parseSoftwareFilters(params: { get(name: string): string | null } | null | undefined): SoftwareFilters {
  return {
    query: (params?.get("q") ?? "").slice(0, MAX_SOFTWARE_QUERY_LENGTH),
    company: params?.get("company") ?? "",
  };
}

// The search string for a page's address and for `GET /v1/software`.
export function softwareParams({ query, company }: SoftwareFilters) {
  const params = new URLSearchParams();
  if (query.trim()) params.set("q", query.trim().slice(0, MAX_SOFTWARE_QUERY_LENGTH));
  if (company) params.set("company", company);
  return params;
}

// `GET /v1/software/devices` for one application, or one version of it.
export function installedOnParams(name: string, version: string | null | undefined, company: string) {
  const params = new URLSearchParams({ name });
  if (version === null) params.set("unversioned", "true");
  else if (version !== undefined) params.set("version", version);
  if (company) params.set("company", company);
  return params;
}
