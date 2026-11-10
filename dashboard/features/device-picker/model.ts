// Choosing devices from the whole inventory, which may hold a hundred
// thousand: matching a search, capping what one menu shows, and the
// one-pass sums the dialogs need. Imports carry the .ts extension so
// node:test can load this module directly.
import { DEFAULT_DEVICE_FILTERS, clampDeviceQuery, filterAgents } from "../agents/device-filters.ts";
import type { Agent, DevicePlatform, Tag } from "../agents/types.ts";

// How many matches the one-device menu lists; typing narrows the rest.
export const DEVICE_MATCH_LIMIT = 50;

// The height of a row in the many-device list, its gap included. The
// stylesheet's `.device-picker-rows label` keeps to it.
export const DEVICE_ROW_HEIGHT = 60;

type Searchable = Pick<Agent, "id" | "name" | "connected" | "company_id" | "system" | "tag_ids">;

// The devices a search matches, in their order, as the Devices list's
// search box matches them; every device for an empty search.
export function searchDevices<T extends Searchable>(agents: readonly T[], query: string, tags: readonly Tag[]): readonly T[] {
  if (!query.trim()) return agents;
  // filterAgents only reads the list.
  return filterAgents(agents as T[], { ...DEFAULT_DEVICE_FILTERS, query: clampDeviceQuery(query) }, tags);
}

// The first `limit` devices a search matches, and how many match in all.
export function matchDevices<T extends Searchable>(agents: readonly T[], query: string, tags: readonly Tag[], limit = DEVICE_MATCH_LIMIT) {
  const matches = searchDevices(agents, query, tags);
  return { shown: matches.slice(0, limit), total: matches.length };
}

// The note under a capped menu, or null when it shows every match.
export function moreMatchesText(shown: number, total: number) {
  return total > shown ? `Showing ${shown} of ${total.toLocaleString()}. Type to narrow them.` : null;
}

// The devices on `platforms`, and any in `alsoIds`, in their order.
export function devicesOn<T extends Pick<Agent, "id" | "platform">>(agents: readonly T[], platforms: readonly DevicePlatform[], alsoIds: readonly string[] = []) {
  const also = new Set(alsoIds);
  return agents.filter((agent) => platforms.includes(agent.platform) || also.has(agent.id));
}

// Of the devices covered, how many are on another platform and are
// skipped, and how many of the rest are offline.
export function platformCounts(covered: readonly Pick<Agent, "platform" | "connected">[], platforms: readonly DevicePlatform[]) {
  let skipped = 0;
  let offline = 0;
  for (const agent of covered) {
    if (!platforms.includes(agent.platform)) skipped++;
    else if (!agent.connected) offline++;
  }
  return { skipped, offline };
}
