// Custom fields: definitions, values and the device filter, as the API
// gives them (see docs/custom-fields.md). Imports carry the .ts extension so
// node:test can load this module directly.

export type FieldType = "text" | "multiline" | "number" | "checkbox" | "date" | "dropdown" | "multi_select" | "url" | "secret";
export type AppliesTo = "devices" | "companies" | "both";
export type ScriptAccess = "none" | "read" | "read_write";
export type FieldOp = "equals" | "contains" | "empty" | "not_empty";

export type FieldDefinition = {
  id: string;
  key: string;
  label: string;
  description: string;
  type: FieldType;
  options: string[];
  applies_to: AppliesTo;
  position: number;
  script_access: ScriptAccess;
  created_at: number;
  updated_at: number;
};

// A value as JSON of its field's type: text, a number, a checkbox's
// boolean, or a multi-select's options.
export type FieldJson = string | number | boolean | string[] | null;

export type FieldAuthor =
  | { kind: "user"; user_id: string; name: string }
  | { kind: "script"; run_id: string; script_name: string };

export type FieldValue = {
  field: FieldDefinition;
  // Always null for a secret, which is revealed on its own.
  value: FieldJson;
  set: boolean;
  source: "device" | "company" | null;
  inherited_from?: { id: string; name: string };
  updated_at: number | null;
  updated_by: FieldAuthor | null;
};

export const FIELD_TYPES: readonly { type: FieldType; label: string }[] = [
  { type: "text", label: "Text" },
  { type: "multiline", label: "Multi-line text" },
  { type: "number", label: "Number" },
  { type: "checkbox", label: "Checkbox" },
  { type: "date", label: "Date" },
  { type: "dropdown", label: "Dropdown" },
  { type: "multi_select", label: "Multi-select" },
  { type: "url", label: "Web address" },
  { type: "secret", label: "Secret" },
];

export const APPLIES_TO: readonly { value: AppliesTo; label: string }[] = [
  { value: "devices", label: "Devices" },
  { value: "companies", label: "Companies" },
  { value: "both", label: "Devices and companies" },
];

export const SCRIPT_ACCESS: readonly { value: ScriptAccess; label: string; help: string }[] = [
  { value: "none", label: "None", help: "Scripts don’t see the field." },
  { value: "read", label: "Read", help: "Scripts read it as an environment variable." },
  { value: "read_write", label: "Read and write", help: "Scripts read it, and set it with meshrmm-agent field set." },
];

export const MAX_KEY_CHARS = 64;
export const MAX_LABEL_CHARS = 80;
export const MAX_DESCRIPTION_CHARS = 500;
export const MAX_OPTIONS = 100;
const MAX_CHARS: Partial<Record<FieldType, number>> = { text: 1000, multiline: 10_000, url: 2048, secret: 4096 };

export const typeLabel = (type: FieldType) => FIELD_TYPES.find((entry) => entry.type === type)?.label ?? type;
export const appliesToDevices = (field: Pick<FieldDefinition, "applies_to">) => field.applies_to !== "companies";
export const appliesToCompanies = (field: Pick<FieldDefinition, "applies_to">) => field.applies_to !== "devices";
export const hasOptions = (type: FieldType) => type === "dropdown" || type === "multi_select";

// A key the label suggests: lowercase words joined by underscores.
export function suggestedKey(label: string) {
  const key = label
    .normalize("NFKD")
    .toLowerCase()
    .replace(/[^a-z0-9]+/g, "_")
    .replace(/^[_0-9]+|_+$/g, "");
  return key.slice(0, MAX_KEY_CHARS).replace(/_+$/, "");
}

export function keyProblem(key: string): string | null {
  if (!key) return "Give the field a key.";
  if (!/^[a-z][a-z0-9_]*$/.test(key)) return "A key starts with a lowercase letter and has only lowercase letters, digits and underscores.";
  if (key.length > MAX_KEY_CHARS) return `A key has at most ${MAX_KEY_CHARS} characters.`;
  return null;
}

// The environment variable a script reads the field from.
export const fieldVariable = (key: string) => `MESHRMM_FIELD_${key.toUpperCase()}`;

// The options typed one per line, tidied, with a problem if any.
export function parseOptions(text: string): { options: string[]; problem: string | null } {
  const options = text.split("\n").map((line) => line.trim()).filter(Boolean);
  const seen = new Set<string>();
  for (const option of options) {
    const folded = option.toLowerCase();
    if (seen.has(folded)) return { options, problem: `“${option}” is there twice.` };
    seen.add(folded);
  }
  if (options.length > MAX_OPTIONS) return { options, problem: `A field has at most ${MAX_OPTIONS} options.` };
  return { options, problem: null };
}

// What an editor holds: text for most types, a boolean for a checkbox
// ("" for no value), and the chosen options for a multi-select.
export type Draft = string | boolean | string[];

export function draftOf(field: FieldDefinition, value: FieldJson): Draft {
  if (field.type === "multi_select") return Array.isArray(value) ? value : [];
  if (field.type === "checkbox") return typeof value === "boolean" ? value : "";
  if (value === null || Array.isArray(value)) return "";
  return String(value);
}

// The draft as JSON the API takes, or why it can't be saved. An empty draft
// clears the value.
export function draftValue(field: FieldDefinition, draft: Draft): { value: FieldJson } | { problem: string } {
  if (field.type === "multi_select") {
    const chosen = Array.isArray(draft) ? draft.filter((option) => field.options.includes(option)) : [];
    return { value: chosen.length ? field.options.filter((option) => chosen.includes(option)) : null };
  }
  if (field.type === "checkbox") return { value: typeof draft === "boolean" ? draft : null };
  const text = typeof draft === "string" ? draft : "";
  const trimmed = text.trim();
  if (field.type === "secret") return text === "" ? { value: null } : lengthChecked(field, text);
  if (!trimmed) return { value: null };
  switch (field.type) {
    case "number": {
      const number = Number(trimmed);
      return Number.isFinite(number) ? { value: number } : { problem: "Enter a number." };
    }
    case "date":
      return isDate(trimmed) ? { value: trimmed } : { problem: "Enter a date as YYYY-MM-DD." };
    case "dropdown": {
      const option = field.options.find((candidate) => candidate.toLowerCase() === trimmed.toLowerCase());
      return option ? { value: option } : { problem: "Choose one of the options." };
    }
    case "url":
      return isWebAddress(trimmed) ? lengthChecked(field, trimmed) : { problem: "Enter a web address starting with https:// or http://." };
    case "text":
      return /[\n\r]/.test(trimmed) ? { problem: "Keep it on one line." } : lengthChecked(field, trimmed);
    default:
      return lengthChecked(field, text.replace(/\r\n/g, "\n").trimEnd());
  }
}

function lengthChecked(field: FieldDefinition, text: string): { value: string } | { problem: string } {
  const max = MAX_CHARS[field.type];
  return max !== undefined && [...text].length > max ? { problem: `Keep it to ${max.toLocaleString("en-US")} characters.` } : { value: text };
}

function isDate(text: string) {
  if (!/^\d{4}-\d{2}-\d{2}$/.test(text)) return false;
  const date = new Date(`${text}T00:00:00Z`);
  return !Number.isNaN(date.getTime()) && date.toISOString().slice(0, 10) === text;
}

export function isWebAddress(text: string) {
  try {
    const url = new URL(text);
    return (url.protocol === "https:" || url.protocol === "http:") && url.hostname !== "";
  } catch {
    return false;
  }
}

// A value as one line of text, for reading.
export function displayValue(field: FieldDefinition, value: FieldJson, set: boolean): string {
  if (field.type === "secret") return set ? "••••••••" : "";
  if (value === null) return "";
  if (Array.isArray(value)) return value.join(", ");
  if (typeof value === "boolean") return value ? "Yes" : "No";
  if (field.type === "number" && typeof value === "number") return value.toLocaleString("en-US", { maximumFractionDigits: 20 });
  return String(value);
}

// Who changed a value, as a sentence fragment.
export function authorText(author: FieldAuthor | null) {
  if (!author) return "";
  return author.kind === "script" ? `the script “${author.script_name}”` : author.name;
}

// Where a device's value comes from.
export function sourceText(value: Pick<FieldValue, "source" | "inherited_from">) {
  if (value.source !== "company" || !value.inherited_from) return null;
  return `From ${value.inherited_from.name}`;
}

// The comparisons the device filter offers for a type.
export function filterOps(type: FieldType): FieldOp[] {
  if (type === "secret") return ["not_empty", "empty"];
  if (type === "multi_select") return ["contains", "not_empty", "empty"];
  if (type === "text" || type === "multiline" || type === "url") return ["contains", "equals", "not_empty", "empty"];
  return ["equals", "not_empty", "empty"];
}

export const OP_LABELS: Record<FieldOp, string> = {
  equals: "is",
  contains: "contains",
  not_empty: "has a value",
  empty: "is empty",
};

export const opNeedsValue = (op: FieldOp) => op === "equals" || op === "contains";

// The device wall's field filter: a field's key, a comparison, and the
// value it compares with.
export type FieldFilter = { key: string; op: FieldOp; value: string };

const OPS: readonly FieldOp[] = ["equals", "contains", "empty", "not_empty"];
const MAX_FILTER_VALUE = 200;

export function parseFieldFilter(params: { get(name: string): string | null } | null | undefined): FieldFilter | null {
  const key = params?.get("field") ?? "";
  const op = params?.get("field_op") as FieldOp | null;
  if (!key || keyProblem(key) || !op || !OPS.includes(op)) return null;
  const value = (params?.get("field_value") ?? "").slice(0, MAX_FILTER_VALUE);
  if (opNeedsValue(op) && !value.trim()) return null;
  return { key, op, value: opNeedsValue(op) ? value : "" };
}

export function writeFieldFilter(params: URLSearchParams, filter: FieldFilter | null) {
  if (!filter) return;
  params.set("field", filter.key);
  params.set("field_op", filter.op);
  if (opNeedsValue(filter.op)) params.set("field_value", filter.value.slice(0, MAX_FILTER_VALUE));
}

export function fieldFilterLabel(filter: FieldFilter | null, fields: readonly FieldDefinition[]) {
  if (!filter) return "Any field";
  const label = fields.find((field) => field.key === filter.key)?.label ?? filter.key;
  return opNeedsValue(filter.op) ? `${label} ${OP_LABELS[filter.op]} “${filter.value}”` : `${label} ${OP_LABELS[filter.op]}`;
}

export const sameFieldFilter = (a: FieldFilter | null, b: FieldFilter | null) =>
  a === b || (a !== null && b !== null && a.key === b.key && a.op === b.op && a.value === b.value);
