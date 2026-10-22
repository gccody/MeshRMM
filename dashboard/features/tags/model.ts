// Tags: free-form labels for grouping devices besides their company. Imports
// carry the .ts extension so node:test can load this module directly.
import type { Agent, Tag, TagColor } from "../agents/types.ts";

export type TagMatch = "any" | "all";

export const MAX_TAG_NAME_CHARS = 60;

export const TAG_COLOR_LABELS: Record<TagColor, string> = {
  gray: "Gray",
  red: "Red",
  orange: "Orange",
  amber: "Amber",
  green: "Green",
  teal: "Teal",
  blue: "Blue",
  purple: "Purple",
  pink: "Pink",
};

// A tag as the Tags page lists it.
export type TagSummary = Tag & {
  device_count: number;
  policy_count: number;
  rule_count: number;
};

// The tags with these IDs, in the list's order (by name). IDs of tags this
// browser doesn't know are left out.
export function tagsOf(ids: readonly string[] | undefined, tags: readonly Tag[]): Tag[] {
  if (!ids?.length) return [];
  const wanted = new Set(ids);
  return tags.filter((tag) => wanted.has(tag.id));
}

// Whether a device has any, or all, of `ids`. No IDs match every device.
export function hasTags(agent: Pick<Agent, "tag_ids">, ids: readonly string[], match: TagMatch) {
  if (!ids.length) return true;
  const own = new Set(agent.tag_ids ?? []);
  return match === "all" ? ids.every((id) => own.has(id)) : ids.some((id) => own.has(id));
}

// Whether a tag's name matches a search box, ignoring case.
export const matchesTagName = (name: string, query: string) =>
  name.toLocaleLowerCase().includes(query.trim().toLocaleLowerCase());

// Unicode control characters, which names may not contain.
// eslint-disable-next-line no-control-regex
const CONTROL = /[\u0000-\u001f\u007f-\u009f]/u;

// Why a tag can't have this name, or null when it can. Names are unique
// ignoring case.
export function tagNameProblem(name: string, tags: readonly Tag[], exceptId: string | null = null): string | null {
  const trimmed = name.trim();
  if (!trimmed) return "Name the tag.";
  if (Array.from(trimmed).length > MAX_TAG_NAME_CHARS || CONTROL.test(trimmed)) {
    return `Use up to ${MAX_TAG_NAME_CHARS} characters, on one line.`;
  }
  const wanted = trimmed.toLocaleLowerCase();
  if (tags.some((tag) => tag.id !== exceptId && tag.name.toLocaleLowerCase() === wanted)) {
    return "A tag with this name already exists.";
  }
  return null;
}

// "a, b or c", the first four names and how many more.
function list(names: readonly string[], joiner: string) {
  if (names.length <= 1) return names[0] ?? "";
  if (names.length <= 4) return `${names.slice(0, -1).join(", ")} ${joiner} ${names[names.length - 1]}`;
  return `${names.slice(0, 4).join(", ")} ${joiner} ${names.length - 4} more`;
}

// Devices tagged with chosen tags, in words: "Tagged London or Servers".
export function describeTagTarget(ids: readonly string[], match: TagMatch, tags: readonly Tag[]) {
  const names = tagsOf(ids, tags).map((tag) => tag.name);
  const missing = ids.length - names.length;
  if (!names.length) return missing ? "Deleted tags" : "No tags";
  return `Tagged ${list(names, match === "all" ? "and" : "or")}`;
}

// Bulk tagging: for each tag, whether all, some or none of the chosen
// devices have it.
export type TagCoverage = "all" | "some" | "none";

export function tagCoverage(agents: readonly Pick<Agent, "tag_ids">[], tagId: string): TagCoverage {
  const count = agents.filter((agent) => agent.tag_ids?.includes(tagId)).length;
  return count === 0 ? "none" : count === agents.length ? "all" : "some";
}

// What a bulk tag edit asks for: each tag the user set to on or off,
// against what the devices have now.
export function tagChanges(agents: readonly Pick<Agent, "tag_ids">[], wanted: ReadonlyMap<string, boolean>) {
  const add: string[] = [];
  const remove: string[] = [];
  for (const [id, on] of wanted) {
    const coverage = tagCoverage(agents, id);
    if (on && coverage !== "all") add.push(id);
    if (!on && coverage !== "none") remove.push(id);
  }
  return { add, remove };
}
