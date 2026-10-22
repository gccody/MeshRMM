/** The operating system a device's Agent runs on. */
export type DevicePlatform = "windows" | "macos" | "linux";

export const PLATFORM_LABELS: Record<DevicePlatform, string> = {
  windows: "Windows",
  macos: "macOS",
  linux: "Linux",
};

/** Only Windows has a background desktop; see docs/background-mode.md. */
export const hasBackgroundDesktop = (agent: Agent) => agent.platform === "windows";

export type Agent = {
  id: string;
  name: string;
  platform: DevicePlatform;
  connected: boolean;
  /** The release an offline Agent went offline to install. */
  updating_to?: string;
  /** The company the device belongs to, or null for none. */
  company_id?: string | null;
  /** When the device enrolled. */
  created_at?: number;
  /** The device list's summary of its latest system information, once it sent one. */
  system?: SystemSummary | null;
};

/** What the device list shows and searches from a device's hardware and software. */
export type SystemSummary = {
  /** The operating system's name and version, such as "Windows 11 Pro 24H2". */
  os: string;
  manufacturer: string | null;
  model: string | null;
  serial_number: string | null;
  /** The adapters' addresses, without loopback and link-local ones. */
  ip_addresses: string[];
  /** The address the Agent last connected from. */
  public_ip: string | null;
  /** When the server received the snapshot. */
  received_at: number;
};

export type Company = {
  id: string;
  name: string;
};

// The devices and the companies they belong to, as the event socket keeps them.
export type Inventory = {
  agents: Agent[];
  companies: Company[];
};

export type AgentSnapshot = {
  type: "snapshot";
  revision: number;
  agents: Agent[];
  companies?: Company[];
  generated_at_unix_ms: number;
};

export type AgentEvent =
  | AgentSnapshot
  | { type: "agent_upsert"; revision: number; agent: Agent }
  | { type: "agent_deleted"; revision: number; agent_id: string }
  | { type: "company_upsert"; revision: number; company: Company }
  | { type: "company_deleted"; revision: number; company_id: string };

export type AgentDelta = Exclude<AgentEvent, AgentSnapshot>;
