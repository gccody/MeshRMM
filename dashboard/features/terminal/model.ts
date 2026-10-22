// Remote terminals: the shells and accounts a device offers, the socket's
// messages and close codes, and the browser's half of the flow control.
// Imports carry the .ts extension so node:test can load this module directly.
import type { Permission } from "../auth/types.ts";
import type { DevicePlatform } from "../agents/types.ts";

export type TerminalShell = "login" | "bash" | "zsh" | "sh" | "powershell" | "pwsh" | "cmd";
export type TerminalAccount = "system" | "user";

export const SHELL_LABELS: Record<TerminalShell, string> = {
  login: "Login shell",
  bash: "bash",
  zsh: "zsh",
  sh: "sh",
  powershell: "Windows PowerShell",
  pwsh: "PowerShell 7",
  cmd: "Command Prompt",
};

const WINDOWS_SHELLS: readonly TerminalShell[] = ["powershell", "pwsh", "cmd"];
const UNIX_SHELLS: readonly TerminalShell[] = ["login", "bash", "zsh", "sh"];

// What every such computer has, offered until the device reports its own.
const DEFAULT_SHELLS: Record<DevicePlatform, readonly TerminalShell[]> = {
  windows: ["powershell", "cmd"],
  macos: ["login", "sh"],
  linux: ["login", "sh"],
};

const isShell = (value: unknown): value is TerminalShell => typeof value === "string" && value in SHELL_LABELS;

// The shells to offer: those the device reported in its system information
// that run on its platform, or the platform's defaults before it has.
export function shellOptions(platform: DevicePlatform, reported: unknown): TerminalShell[] {
  const runs = platform === "windows" ? WINDOWS_SHELLS : UNIX_SHELLS;
  const listed = Array.isArray(reported) ? reported.filter(isShell).filter((shell) => runs.includes(shell)) : [];
  const unique = [...new Set(listed)];
  return unique.length > 0 ? unique : [...DEFAULT_SHELLS[platform]];
}

// The device's own account is SYSTEM on Windows and root elsewhere.
export function accountLabel(account: TerminalAccount, platform: DevicePlatform) {
  if (account === "user") return "Signed-in user";
  return platform === "windows" ? "SYSTEM" : "root";
}

export const ACCOUNT_PERMISSIONS: Record<TerminalAccount, Permission> = {
  system: "terminal.connect_system",
  user: "terminal.connect",
};

// The accounts the user may open terminals as, the device's own first.
export function accountOptions(can: (permission: Permission) => boolean): TerminalAccount[] {
  return (["system", "user"] as const).filter((account) => can(ACCOUNT_PERMISSIONS[account]));
}

export type TerminalChoice = { shell: TerminalShell; account: TerminalAccount };

// The choice to start from: the remembered one where it is still offered,
// else the first shell and account.
export function initialChoice(
  shells: readonly TerminalShell[],
  accounts: readonly TerminalAccount[],
  remembered: Partial<TerminalChoice> | null,
): TerminalChoice | null {
  if (shells.length === 0 || accounts.length === 0) return null;
  return {
    shell: remembered?.shell && shells.includes(remembered.shell) ? remembered.shell : shells[0],
    account: remembered?.account && accounts.includes(remembered.account) ? remembered.account : accounts[0],
  };
}

// Reads a remembered choice, ignoring anything this page didn't write.
export function parseRememberedChoice(text: string | null): Partial<TerminalChoice> | null {
  if (!text) return null;
  try {
    const value: unknown = JSON.parse(text);
    if (!value || typeof value !== "object") return null;
    const { shell, account } = value as Record<string, unknown>;
    return {
      shell: isShell(shell) ? shell : undefined,
      account: account === "system" || account === "user" ? account : undefined,
    };
  } catch {
    return null;
  }
}

export const rememberedChoiceKey = (platform: DevicePlatform) => `meshrmm.terminal.${platform}`;

export const MAX_COLUMNS = 1000;
export const MAX_ROWS = 500;

// Whether the server accepts a terminal of this size.
export function isValidSize(columns: number, rows: number) {
  return Number.isInteger(columns) && Number.isInteger(rows) && columns >= 1 && columns <= MAX_COLUMNS && rows >= 1 && rows <= MAX_ROWS;
}

// A size the server accepts: a terminal too small to measure yet opens at
// 80 by 24 and is resized once it is laid out.
export function clampSize(columns: number, rows: number): { columns: number; rows: number } {
  if (!Number.isFinite(columns) || !Number.isFinite(rows) || columns < 1 || rows < 1) return { columns: 80, rows: 24 };
  return {
    columns: Math.min(MAX_COLUMNS, Math.max(1, Math.floor(columns))),
    rows: Math.min(MAX_ROWS, Math.max(1, Math.floor(rows))),
  };
}

// What the device says on the socket's text messages.
export type TerminalEvent =
  | { type: "started"; account: string; shell: string }
  | { type: "failed"; message: string }
  | { type: "exited"; code: number | null };

export function parseTerminalEvent(text: string): TerminalEvent | null {
  try {
    const value: unknown = JSON.parse(text);
    if (!value || typeof value !== "object") return null;
    const event = value as Record<string, unknown>;
    if (event.type === "started" && typeof event.account === "string" && typeof event.shell === "string") {
      return { type: "started", account: event.account, shell: event.shell };
    }
    if (event.type === "failed" && typeof event.message === "string") return { type: "failed", message: event.message };
    if (event.type === "exited") return { type: "exited", code: typeof event.code === "number" ? event.code : null };
    return null;
  } catch {
    return null;
  }
}

// A terminal's state, as its tab and status line show it.
export type TerminalStatus =
  | { state: "connecting" }
  | { state: "connected"; account: string; shell: string }
  | { state: "disconnected"; reason: string }
  | { state: "exited"; code: number | null }
  | { state: "failed"; message: string };

export function statusText(status: TerminalStatus) {
  switch (status.state) {
    case "connecting": return "Connecting…";
    case "connected": return `Connected · ${status.account} · ${status.shell}`;
    case "disconnected": return `Disconnected: ${status.reason}`;
    case "exited": return status.code === null ? "Exited" : `Exited (code ${status.code})`;
    case "failed": return `Couldn’t start: ${status.message}`;
  }
}

export const isEnded = (status: TerminalStatus) => status.state !== "connecting" && status.state !== "connected";

const CLOSE_MESSAGES: Record<number, string> = {
  1000: "The terminal was closed",
  4001: "Sign in again",
  4003: "You may no longer open this terminal",
  4004: "The device didn’t open the terminal",
  4008: "Closed for lack of input",
  4009: "The device disconnected",
  4010: "This terminal has ended or is open elsewhere",
};

// Why the socket closed: the server's own words when it gave some.
export function closeReason(code: number, reason: string) {
  const text = reason.trim();
  if (text) return text[0].toUpperCase() + text.slice(1);
  return CLOSE_MESSAGES[code] ?? "The connection to the server was lost";
}

// The most input one socket message carries.
export const MAX_INPUT_BYTES = 16 * 1024;
// The largest paste the website sends; the device keeps at most 1 MiB of
// input waiting for the shell.
export const MAX_PASTE_BYTES = 1024 * 1024;
export const PASTE_TOO_LARGE = "That paste is larger than 1 MB, so it wasn’t sent. Paste it in parts, or send it as a file.";

// Whether input is more than the website sends at once.
export const isTooLarge = (messages: readonly Uint8Array[]) =>
  messages.reduce((total, message) => total + message.length, 0) > MAX_PASTE_BYTES;

const encoder = new TextEncoder();

// Input as UTF-8 messages of at most `limit` bytes, never splitting a
// character. xterm's onData gives text; onBinary gives one byte per
// character, which `binary` keeps as is.
export function inputMessages(data: string, binary = false, limit = MAX_INPUT_BYTES): Uint8Array[] {
  const bytes = binary ? Uint8Array.from(data, (character) => character.charCodeAt(0) & 0xff) : encoder.encode(data);
  const messages: Uint8Array[] = [];
  let start = 0;
  while (start < bytes.length) {
    let end = Math.min(bytes.length, start + limit);
    // Back up to the start of a UTF-8 character (continuation bytes are 10xxxxxx).
    if (!binary && end < bytes.length) {
      while (end > start && (bytes[end] & 0xc0) === 0x80) end -= 1;
      if (end === start) end = Math.min(bytes.length, start + limit);
    }
    messages.push(bytes.subarray(start, end));
    start = end;
  }
  return messages;
}

// Output the browser has drawn and not yet acknowledged. The device stops
// sending once it has 512 KiB unacknowledged, so the browser acknowledges as
// xterm finishes each write: at once after 32 KiB, or after a short pause
// for less.
export const ACK_THRESHOLD_BYTES = 32 * 1024;
export const ACK_DELAY_MS = 100;

export class AckCounter {
  private processed = 0;
  private acknowledged = 0;

  // Records `bytes` more drawn output. Returns the total to acknowledge now,
  // or null to wait for more or for the pause.
  drawn(bytes: number): number | null {
    this.processed += bytes;
    return this.processed - this.acknowledged >= ACK_THRESHOLD_BYTES ? this.take() : null;
  }

  // The total to acknowledge after a pause, or null when it's all acknowledged.
  flush(): number | null {
    return this.processed > this.acknowledged ? this.take() : null;
  }

  get pending() {
    return this.processed - this.acknowledged;
  }

  private take() {
    this.acknowledged = this.processed;
    return this.processed;
  }
}

export const ackMessage = (bytes: number) => JSON.stringify({ type: "ack", bytes });
export const resizeMessage = (columns: number, rows: number) => JSON.stringify({ type: "resize", columns, rows });

export const terminalSocketUrl = (location: Pick<Location, "protocol" | "host">, id: string) =>
  `${location.protocol === "https:" ? "wss:" : "ws:"}//${location.host}/v1/terminals/${encodeURIComponent(id)}`;

export const terminalPagePath = (deviceId: string) => `/terminal?id=${encodeURIComponent(deviceId)}`;
