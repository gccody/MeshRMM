// The registry tool's data and the editor's rules: a Windows device's keys
// and values as the server sends them. Imports carry the .ts extension so
// node:test can load this module directly.

export type RegistryRoot = "HKLM" | "HKU" | "HKCR" | "HKCC";
export type RegistryView = "64" | "32";

export const REGISTRY_ROOTS: readonly RegistryRoot[] = ["HKLM", "HKU", "HKCR", "HKCC"];

export const ROOT_NAMES: Record<RegistryRoot, string> = {
  HKLM: "HKEY_LOCAL_MACHINE",
  HKU: "HKEY_USERS",
  HKCR: "HKEY_CLASSES_ROOT",
  HKCC: "HKEY_CURRENT_CONFIG",
};

export type RegistryKey = { root: RegistryRoot; path: string; view: RegistryView };

export type RegistryData =
  | { type: "string"; data: string }
  | { type: "expand_string"; data: string }
  | { type: "multi_string"; data: string[] }
  | { type: "dword"; data: number }
  | { type: "qword"; data: string }
  | { type: "binary"; data: string }
  | { type: "other"; data: { kind: number; hex: string } };

export type RegistryType = RegistryData["type"];

export type RegistryValue = RegistryData & { name: string; size: number; truncated?: boolean };

export type RegistrySubkey = { name: string; has_subkeys?: boolean };

export type RegistryListing = {
  key: RegistryKey;
  subkeys: RegistrySubkey[];
  values: RegistryValue[];
  modified_at?: number;
  truncated?: boolean;
};

// The types the editor writes, in Regedit's order.
export const WRITABLE_TYPES: readonly Exclude<RegistryType, "other">[] = ["string", "binary", "dword", "qword", "multi_string", "expand_string"];

export const TYPE_NAMES: Record<Exclude<RegistryType, "other">, string> = {
  string: "REG_SZ",
  expand_string: "REG_EXPAND_SZ",
  multi_string: "REG_MULTI_SZ",
  dword: "REG_DWORD",
  qword: "REG_QWORD",
  binary: "REG_BINARY",
};

const OTHER_NAMES: Record<number, string> = {
  0: "REG_NONE",
  5: "REG_DWORD_BIG_ENDIAN",
  6: "REG_LINK",
  8: "REG_RESOURCE_LIST",
  9: "REG_FULL_RESOURCE_DESCRIPTOR",
  10: "REG_RESOURCE_REQUIREMENTS_LIST",
};

export const typeName = (data: RegistryData) =>
  data.type === "other" ? OTHER_NAMES[data.data.kind] ?? `Type ${data.data.kind}` : TYPE_NAMES[data.type];

// The most data a value holds that the editor writes, in bytes as stored.
export const MAX_REGISTRY_VALUE_BYTES = 256 * 1024;
const MAX_KEY_NAME_CHARS = 255;

// A key's names below its root.
export const pathNames = (path: string) => path.split("\\").filter(Boolean);

export const childPath = (path: string, name: string) => (path ? `${path}\\${name}` : name);

export const parentPath = (path: string) => pathNames(path).slice(0, -1).join("\\");

// Its full name, as Regedit's address bar shows it.
export const keyDisplay = (key: Pick<RegistryKey, "root" | "path">) =>
  key.path ? `${ROOT_NAMES[key.root]}\\${key.path}` : ROOT_NAMES[key.root];

const ROOT_ALIASES: Record<string, RegistryRoot> = {
  HKLM: "HKLM",
  HKEY_LOCAL_MACHINE: "HKLM",
  HKU: "HKU",
  HKEY_USERS: "HKU",
  HKCR: "HKCR",
  HKEY_CLASSES_ROOT: "HKCR",
  HKCC: "HKCC",
  HKEY_CURRENT_CONFIG: "HKCC",
};

// A key typed or pasted, as `HKLM\SOFTWARE`, `HKEY_LOCAL_MACHINE\SOFTWARE`
// or Regedit's `Computer\HKEY_LOCAL_MACHINE\SOFTWARE`; null when it names
// no root the tool browses. HKEY_CURRENT_USER is each user's, under
// HKEY_USERS.
export function parseKeyPath(text: string): { root: RegistryRoot; path: string } | null {
  const names = text.trim().replace(/\//g, "\\").split("\\").filter(Boolean);
  if (names[0]?.toLowerCase() === "computer") names.shift();
  const root = ROOT_ALIASES[(names.shift() ?? "").toUpperCase()];
  return root ? { root, path: names.join("\\") } : null;
}

// Why `name` can't be a new key's name, or null when it can.
export function keyNameProblem(name: string): string | null {
  if (!name) return "Enter a name.";
  if (name.includes("\\")) return "A key’s name can’t contain a backslash.";
  if ([...name].length > MAX_KEY_NAME_CHARS) return `A key’s name has at most ${MAX_KEY_NAME_CHARS} characters.`;
  if (/\p{Cc}/u.test(name)) return "A key’s name can’t contain control characters.";
  return null;
}

// Pairs of hex digits, spaced, as Regedit shows binary data.
const spacedHex = (hex: string) => hex.match(/.{1,2}/g)?.join(" ") ?? "";

const hexNumber = (value: bigint, digits: number) => `0x${value.toString(16).padStart(digits, "0")}`;

// A value's data on one line, as Regedit's Data column shows it.
export function formatData(value: RegistryValue): string {
  const more = value.truncated ? "…" : "";
  switch (value.type) {
    case "string":
    case "expand_string":
      return value.name === "" && value.data === "" && value.size === 0 ? "(value not set)" : value.data + more;
    case "multi_string":
      return value.data.join(" ") + more;
    case "dword":
      return `${hexNumber(BigInt(value.data), 8)} (${value.data})`;
    case "qword":
      return `${hexNumber(BigInt(value.data), 16)} (${value.data})`;
    case "binary":
      return value.data ? spacedHex(value.data) + more : "(zero-length binary value)";
    case "other":
      return value.data.hex ? spacedHex(value.data.hex) + more : "(no data)";
  }
}

// Whether the editor can open `value`: it holds whole, writable data.
export const editable = (value: RegistryValue) => value.type !== "other" && !value.truncated;

export type NumberBase = "hex" | "decimal";

// What the editor shows for `data`: its text, numbers in `base`.
export function draftOf(data: RegistryData, base: NumberBase = "hex"): string {
  switch (data.type) {
    case "string":
    case "expand_string":
      return data.data;
    case "multi_string":
      return data.data.join("\n");
    case "dword":
    case "qword":
      return numberText(BigInt(data.data), base);
    case "binary":
      return spacedHex(data.data);
    case "other":
      return spacedHex(data.data.hex);
  }
}

const numberText = (value: bigint, base: NumberBase) => (base === "hex" ? value.toString(16) : value.toString(10));

// A number typed in `base` as the editor shows it, converted: what the
// field holds after the base is switched.
export function rebase(text: string, from: NumberBase, to: NumberBase): string {
  const value = parseNumber(text, from);
  return value === null ? text : numberText(value, to);
}

function parseNumber(text: string, base: NumberBase): bigint | null {
  const trimmed = text.trim().replace(/^0x/i, "");
  if (!trimmed) return 0n;
  if (base === "hex" ? !/^[0-9a-f]+$/i.test(trimmed) : !/^\d+$/.test(trimmed)) return null;
  return BigInt(base === "hex" ? `0x${trimmed}` : trimmed);
}

const utf16Units = (text: string) => text.length;

// The bytes the registry stores `data` in, as the Agent counts them.
export function storedBytes(data: RegistryData): number {
  switch (data.type) {
    case "string":
    case "expand_string":
      return (utf16Units(data.data) + 1) * 2;
    case "multi_string":
      return (data.data.reduce((sum, text) => sum + utf16Units(text) + 1, 0) + 1) * 2;
    case "dword":
      return 4;
    case "qword":
      return 8;
    case "binary":
      return data.data.length / 2;
    case "other":
      return data.data.hex.length / 2;
  }
}

// The data the editor's `text` spells for `type`, or why it can't be
// written. A multi-string's empty lines are dropped, as Regedit does.
export function dataFromDraft(type: Exclude<RegistryType, "other">, text: string, base: NumberBase): { data: RegistryData } | { problem: string } {
  let data: RegistryData;
  switch (type) {
    case "string":
    case "expand_string":
      if (text.includes("\0")) return { problem: "A string can’t contain a NUL character." };
      data = { type, data: text };
      break;
    case "multi_string":
      data = { type, data: text.split(/\r?\n/).filter((line) => line !== "") };
      break;
    case "dword":
    case "qword": {
      const value = parseNumber(text, base);
      const limit = type === "dword" ? 0xffff_ffffn : 0xffff_ffff_ffff_ffffn;
      if (value === null) return { problem: base === "hex" ? "Enter a hexadecimal number." : "Enter a whole number." };
      if (value > limit) return { problem: `A ${TYPE_NAMES[type]} holds at most ${base === "hex" ? limit.toString(16) : limit.toString(10)}.` };
      data = type === "dword" ? { type, data: Number(value) } : { type, data: value.toString(10) };
      break;
    }
    case "binary": {
      const hex = text.replace(/\s+/g, "").toLowerCase();
      if (!/^[0-9a-f]*$/.test(hex)) return { problem: "Enter hexadecimal bytes, such as 0a ff 10." };
      if (hex.length % 2) return { problem: "Each byte is two hexadecimal digits." };
      data = { type, data: hex };
      break;
    }
  }
  const bytes = storedBytes(data);
  if (bytes > MAX_REGISTRY_VALUE_BYTES) return { problem: `That’s ${bytes.toLocaleString()} bytes; the registry tool writes at most ${MAX_REGISTRY_VALUE_BYTES.toLocaleString()}.` };
  return { data };
}

// Values as Regedit lists them: the default value first, then by name.
export const sortedValues = (values: readonly RegistryValue[]) =>
  [...values].sort((a, b) => (a.name === "" ? -1 : b.name === "" ? 1 : a.name.localeCompare(b.name, undefined, { sensitivity: "base" })));

// A name for a new value or key that `taken` doesn't hold, as Regedit
// suggests: `New Value #1`, `New Key #1`…
export function freshName(prefix: string, taken: readonly string[]): string {
  const lower = new Set(taken.map((name) => name.toLowerCase()));
  for (let index = 1; ; index += 1) {
    const name = `${prefix} #${index}`;
    if (!lower.has(name.toLowerCase())) return name;
  }
}
