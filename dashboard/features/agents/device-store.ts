// The devices, companies and tags the website holds, and each online
// device's latest resource reading. It lives outside React so that a change
// costs what it changed: a device's change moves only that device in the
// list, a reading updates only its device, and subscribers hear only of what
// they follow, at most once a frame however many changes arrive. No React or
// DOM here, so node:test loads it directly.
import type { MetricsReading } from "../metrics/model.ts";
import { compareAgents, compareByName } from "./model.ts";
import type { Agent, AgentDelta, Company, Inventory, Tag } from "./types.ts";

type Listener = () => void;

// Calls `flush` soon. The store asks again only after it ran.
export type Scheduler = (flush: () => void) => void;

// What a subscriber can follow, besides one device or one device's reading:
// `agents` is any device's change (the list and the counts), `ids` a device
// added or removed, and `meta` loading and when anything last changed.
export type Topic = "agents" | "ids" | "companies" | "tags" | "meta";

export type DeviceCounts = { total: number; online: number };

// The devices by ID as they are now; a new lookup once any changed.
export type DeviceLookup = { get(id: string): Agent | undefined };

type Options = {
  schedule?: Scheduler;
  // For tests, to count the work done.
  compare?: (left: Agent, right: Agent) => number;
  now?: () => number;
};

// Hidden tabs run no animation frames, so a timer stands in for them: a few
// times a second at most, and browsers slow it further.
const HIDDEN_FLUSH_MS = 250;

// The next frame while the tab is shown, or the timer while it is hidden,
// whichever comes first. Touches the browser only when called, so importing
// this module while prerendering is safe.
export const frameScheduler: Scheduler = (flush) => {
  let done = false;
  const run = () => {
    if (done) return;
    done = true;
    cancelAnimationFrame(frame);
    clearTimeout(timer);
    flush();
  };
  const frame = requestAnimationFrame(run);
  const timer = setTimeout(run, HIDDEN_FLUSH_MS);
};

// Up to this many devices changed since the list was last read are moved one
// at a time; more, as when a fleet reconnects, are sorted and merged in one
// pass over the list.
const MOVE_LIMIT = 64;

// The index of the first item in `sorted` not before `item`.
function lowerBound<T>(sorted: readonly T[], item: T, compare: (left: T, right: T) => number) {
  let low = 0;
  let high = sorted.length;
  while (low < high) {
    const middle = (low + high) >>> 1;
    if (compare(sorted[middle], item) < 0) low = middle + 1;
    else high = middle;
  }
  return low;
}

function merge<T>(left: readonly T[], right: readonly T[], compare: (left: T, right: T) => number) {
  const merged: T[] = [];
  let i = 0;
  let j = 0;
  while (i < left.length && j < right.length) merged.push(compare(right[j], left[i]) < 0 ? right[j++] : left[i++]);
  while (i < left.length) merged.push(left[i++]);
  while (j < right.length) merged.push(right[j++]);
  return merged;
}

// A sorted list of named items with `item` in place of the one with its ID.
function upsertNamed<T extends Company | Tag>(items: readonly T[], item: T) {
  const next = items.filter((candidate) => candidate.id !== item.id);
  next.splice(lowerBound(next, item, compareByName), 0, item);
  return next;
}

export class DeviceStore {
  #schedule: Scheduler;
  #compare: (left: Agent, right: Agent) => number;
  #now: () => number;

  #agents = new Map<string, Agent>();
  // The list in the order it is shown, as last read, and the devices changed
  // since: each one's entry in that list, or null if it has none. The list
  // is brought up to date when it is next read.
  #order: readonly Agent[] = [];
  #moved = new Map<string, Agent | null>();
  #online = 0;
  #counts: DeviceCounts = { total: 0, online: 0 };
  #lookup: DeviceLookup | null = null;
  #companies: readonly Company[] = [];
  #tags: readonly Tag[] = [];
  #loaded = false;
  #updatedAt: number | null = null;
  #lastUpdated: Date | null = null;
  // Only online devices' readings; one going offline or away takes its own.
  #readings = new Map<string, MetricsReading>();

  #listeners = new Map<Topic, Set<Listener>>();
  #deviceListeners = new Map<string, Set<Listener>>();
  #readingListeners = new Map<string, Set<Listener>>();
  #changedTopics = new Set<Topic>();
  #changedDevices = new Set<string>();
  #changedReadings = new Set<string>();
  #scheduled = false;

  constructor({ schedule = frameScheduler, compare = compareAgents, now = Date.now }: Options = {}) {
    this.#schedule = schedule;
    this.#compare = compare;
    this.#now = now;
  }

  // A snapshot or a loaded list: replaces everything.
  replace({ agents, companies, tags }: Inventory) {
    this.#agents = new Map(agents.map((agent) => [agent.id, agent]));
    this.#order = [...this.#agents.values()].sort(this.#compare);
    this.#moved = new Map();
    this.#online = 0;
    for (const agent of this.#agents.values()) if (agent.connected) this.#online++;
    this.#companies = [...companies].sort(compareByName);
    this.#tags = [...tags].sort(compareByName);
    this.#loaded = true;
    for (const id of this.#readings.keys()) {
      if (!this.#agents.get(id)?.connected) this.#dropReading(id);
    }
    for (const id of this.#deviceListeners.keys()) this.#changedDevices.add(id);
    this.#changed("agents", "ids", "companies", "tags");
  }

  // One change, in revision order.
  apply(change: AgentDelta) {
    switch (change.type) {
      case "agent_upsert":
        this.#upsert(change.agent);
        break;
      case "agent_deleted":
        this.#delete(change.agent_id);
        break;
      case "company_upsert":
        this.#companies = upsertNamed(this.#companies, change.company);
        this.#changed("companies");
        break;
      case "company_deleted":
        this.#companies = this.#companies.filter((company) => company.id !== change.company_id);
        this.#changed("companies");
        break;
      case "tag_upsert":
        this.#tags = upsertNamed(this.#tags, change.tag);
        this.#changed("tags");
        break;
      case "tag_deleted":
        this.#tags = this.#tags.filter((tag) => tag.id !== change.tag_id);
        this.#changed("tags");
        break;
    }
  }

  // The latest readings of the devices that reported. A device that is
  // offline, or not yet known, has none: the server sends every online
  // device's reading again after each snapshot.
  applyReadings(readings: readonly MetricsReading[]) {
    for (const reading of readings) {
      const id = reading.device_id;
      if (!this.#agents.get(id)?.connected) continue;
      this.#readings.set(id, reading);
      if (this.#readingListeners.has(id)) this.#changedReadings.add(id);
    }
    this.#request();
  }

  // Forgets everything, for sign-out and the session lock.
  clear() {
    this.#agents = new Map();
    this.#order = [];
    this.#moved = new Map();
    this.#online = 0;
    this.#companies = [];
    this.#tags = [];
    this.#loaded = false;
    this.#updatedAt = null;
    this.#readings = new Map();
    this.#lookup = null;
    for (const id of this.#deviceListeners.keys()) this.#changedDevices.add(id);
    for (const id of this.#readingListeners.keys()) this.#changedReadings.add(id);
    this.#changedTopics.add("agents").add("ids").add("companies").add("tags").add("meta");
    this.#request();
  }

  get(id: string): Agent | undefined {
    return this.#agents.get(id);
  }

  has(id: string) {
    return this.#agents.has(id);
  }

  // Every device, in the order the list shows them. The same array until a
  // device changes.
  agents(): readonly Agent[] {
    if (this.#moved.size) this.#sort();
    return this.#order;
  }

  // For pages that look up the devices they show, such as by name or
  // whether they're online, and redraw when any changes, without a map of
  // their own.
  lookup(): DeviceLookup {
    if (!this.#lookup) {
      const agents = this.#agents;
      this.#lookup = { get: (id) => agents.get(id) };
    }
    return this.#lookup;
  }

  counts(): DeviceCounts {
    const total = this.#agents.size;
    if (this.#counts.total !== total || this.#counts.online !== this.#online) this.#counts = { total, online: this.#online };
    return this.#counts;
  }

  companies(): readonly Company[] {
    return this.#companies;
  }

  tags(): readonly Tag[] {
    return this.#tags;
  }

  // Whether a snapshot or a loaded list has arrived since the store was
  // made or cleared.
  loaded() {
    return this.#loaded;
  }

  lastUpdated(): Date | null {
    if (this.#updatedAt === null) return null;
    if (this.#lastUpdated?.getTime() !== this.#updatedAt) this.#lastUpdated = new Date(this.#updatedAt);
    return this.#lastUpdated;
  }

  reading(id: string): MetricsReading | undefined {
    return this.#readings.get(id);
  }

  subscribe(topic: Topic, listener: Listener) {
    return listen(this.#listeners, topic, listener);
  }

  subscribeDevice(id: string, listener: Listener) {
    return listen(this.#deviceListeners, id, listener);
  }

  subscribeReading(id: string, listener: Listener) {
    return listen(this.#readingListeners, id, listener);
  }

  #upsert(agent: Agent) {
    const previous = this.#agents.get(agent.id);
    if (!this.#moved.has(agent.id)) this.#moved.set(agent.id, previous ?? null);
    this.#agents.set(agent.id, agent);
    this.#online += Number(agent.connected) - Number(previous?.connected ?? false);
    if (!agent.connected) this.#dropReading(agent.id);
    if (this.#deviceListeners.has(agent.id)) this.#changedDevices.add(agent.id);
    if (previous) this.#changed("agents");
    else this.#changed("agents", "ids");
  }

  #delete(id: string) {
    const previous = this.#agents.get(id);
    if (!previous) return;
    if (!this.#moved.has(id)) this.#moved.set(id, previous);
    this.#agents.delete(id);
    if (previous.connected) this.#online--;
    this.#dropReading(id);
    if (this.#deviceListeners.has(id)) this.#changedDevices.add(id);
    this.#changed("agents", "ids");
  }

  #dropReading(id: string) {
    if (this.#readings.delete(id) && this.#readingListeners.has(id)) this.#changedReadings.add(id);
  }

  // Brings the list up to date with the devices changed since it was read,
  // as a new array so subscribers see that it changed.
  #sort() {
    const moved = this.#moved;
    this.#moved = new Map();
    if (moved.size > MOVE_LIMIT) {
      const kept = this.#order.filter((agent) => !moved.has(agent.id));
      const changed: Agent[] = [];
      for (const id of moved.keys()) {
        const agent = this.#agents.get(id);
        if (agent) changed.push(agent);
      }
      this.#order = merge(kept, changed.sort(this.#compare), this.#compare);
      return;
    }
    const order = this.#order.slice();
    for (const [id, previous] of moved) {
      const agent = this.#agents.get(id);
      let at = previous ? lowerBound(order, previous, this.#compare) : -1;
      // Only if two IDs collate as equal.
      if (previous && order[at] !== previous) at = order.indexOf(previous);
      // A change that leaves the device where it was, such as new tags.
      if (agent && previous && this.#compare(agent, previous) === 0) {
        order[at] = agent;
        continue;
      }
      if (previous) order.splice(at, 1);
      if (agent) order.splice(lowerBound(order, agent, this.#compare), 0, agent);
    }
    this.#order = order;
  }

  #changed(...topics: Topic[]) {
    for (const topic of topics) this.#changedTopics.add(topic);
    if (topics.includes("agents")) this.#lookup = null;
    this.#changedTopics.add("meta");
    this.#updatedAt = this.#now();
    this.#request();
  }

  #request() {
    if (this.#scheduled) return;
    if (!this.#changedTopics.size && !this.#changedDevices.size && !this.#changedReadings.size) return;
    this.#scheduled = true;
    this.#schedule(this.#flush);
  }

  #flush = () => {
    this.#scheduled = false;
    const topics = this.#changedTopics;
    const devices = this.#changedDevices;
    const readings = this.#changedReadings;
    this.#changedTopics = new Set();
    this.#changedDevices = new Set();
    this.#changedReadings = new Set();
    for (const topic of topics) notify(this.#listeners.get(topic));
    for (const id of devices) notify(this.#deviceListeners.get(id));
    for (const id of readings) notify(this.#readingListeners.get(id));
  };
}

function listen<K>(listeners: Map<K, Set<Listener>>, key: K, listener: Listener) {
  let set = listeners.get(key);
  if (!set) {
    set = new Set();
    listeners.set(key, set);
  }
  set.add(listener);
  return () => {
    set.delete(listener);
    if (!set.size && listeners.get(key) === set) listeners.delete(key);
  };
}

function notify(listeners: Set<Listener> | undefined) {
  if (!listeners) return;
  for (const listener of [...listeners]) listener();
}
