import { AuthenticationRequired, type AuthorizedFetch } from "../../lib/http.ts";

// The Agent uploads an image of its main display this often.
export const THUMBNAIL_INTERVAL_MS = 5 * 60_000;
// Checking this long after the Agent's next upload is due finds it.
const UPLOAD_GRACE_MS = 20_000;
// Never check one device more often than this.
const MIN_CHECK_DELAY_MS = 10_000;

export type Thumbnail = {
  url: string;
  // When the server received the image.
  updatedAt: Date | null;
};

type Entry = {
  thumbnail: Thumbnail | null;
  etag: string | null;
  checked: boolean;
  // Checked while the device was offline; it uploads again when it connects.
  checkedOffline: boolean;
  nextCheckAt: number;
  loading: Promise<void> | null;
  listeners: Set<() => void>;
};

type Options = {
  fetch: AuthorizedFetch;
  createUrl?: (blob: Blob) => string;
  revokeUrl?: (url: string) => void;
  now?: () => number;
};

// When to look for a newer image of an online device: just after the Agent's
// next upload. An image older than one interval means the screen has not
// changed, which the Agent does not upload again, so wait a full interval.
export function nextCheckAt(now: number, updatedAt: number | null) {
  const due = updatedAt === null ? now : updatedAt + THUMBNAIL_INTERVAL_MS + UPLOAD_GRACE_MS;
  if (due <= now) return now + THUMBNAIL_INTERVAL_MS;
  return Math.max(now + MIN_CHECK_DELAY_MS, Math.min(due, now + THUMBNAIL_INTERVAL_MS));
}

// The latest screen image of each device, shared by every row that shows it.
// Images are revalidated with their ETag, so an unchanged one is not
// downloaded again. Only visible rows ask for them.
export class ThumbnailStore {
  #entries = new Map<string, Entry>();
  #generation = 0;
  #fetch: AuthorizedFetch;
  #createUrl: (blob: Blob) => string;
  #revokeUrl: (url: string) => void;
  #now: () => number;

  constructor({ fetch, createUrl = (blob) => URL.createObjectURL(blob), revokeUrl = (url) => URL.revokeObjectURL(url), now = Date.now }: Options) {
    this.#fetch = fetch;
    this.#createUrl = createUrl;
    this.#revokeUrl = revokeUrl;
    this.#now = now;
  }

  #entry(id: string) {
    let entry = this.#entries.get(id);
    if (!entry) {
      entry = { thumbnail: null, etag: null, checked: false, checkedOffline: false, nextCheckAt: 0, loading: null, listeners: new Set() };
      this.#entries.set(id, entry);
    }
    return entry;
  }

  #publish(entry: Entry, thumbnail: Thumbnail | null) {
    if (entry.thumbnail) this.#revokeUrl(entry.thumbnail.url);
    entry.thumbnail = thumbnail;
    for (const listener of entry.listeners) listener();
  }

  // Uses the website's current request function from now on.
  setFetch(fetch: AuthorizedFetch) {
    this.#fetch = fetch;
  }

  snapshot(id: string): Thumbnail | null {
    return this.#entries.get(id)?.thumbnail ?? null;
  }

  subscribe(id: string, listener: () => void) {
    const entry = this.#entry(id);
    entry.listeners.add(listener);
    return () => entry.listeners.delete(listener);
  }

  // Loads the device's image if a check is due, and returns how long until
  // the next one, or null when there is none: an offline device uploads
  // nothing, so its last image is loaded once, and a locked or cleared
  // website loads nothing more.
  async refresh(id: string, online: boolean): Promise<number | null> {
    const entry = this.#entry(id);
    if (online && entry.checkedOffline) {
      entry.checkedOffline = false;
      entry.nextCheckAt = Math.min(entry.nextCheckAt, this.#now() + UPLOAD_GRACE_MS);
    }
    if (entry.loading) await entry.loading;
    else if (!entry.checked || (online && this.#now() >= entry.nextCheckAt)) {
      entry.loading = this.#load(id, entry, online).finally(() => { entry.loading = null; });
      await entry.loading;
    }
    if (!online || !entry.checked || this.#entries.get(id) !== entry) return null;
    return Math.max(0, entry.nextCheckAt - this.#now());
  }

  async #load(id: string, entry: Entry, online: boolean) {
    const generation = this.#generation;
    const current = () => generation === this.#generation && this.#entries.get(id) === entry;
    let updatedAt: number | null = null;
    try {
      const response = await this.#fetch(`/v1/agents/${encodeURIComponent(id)}/thumbnail`, {
        cache: "no-store",
        headers: entry.etag ? { "If-None-Match": entry.etag } : {},
      });
      if (!current()) {
        await response.body?.cancel();
        return;
      }
      if (response.status === 304) {
        updatedAt = entry.thumbnail?.updatedAt?.getTime() ?? null;
      } else if (response.status === 204) {
        await response.body?.cancel();
        entry.etag = null;
        if (entry.thumbnail) this.#publish(entry, null);
      } else if (response.ok) {
        const blob = await response.blob();
        if (!current()) return;
        const modified = Date.parse(response.headers.get("Last-Modified") ?? "");
        updatedAt = Number.isNaN(modified) ? null : modified;
        entry.etag = response.headers.get("ETag");
        this.#publish(entry, { url: this.#createUrl(blob), updatedAt: updatedAt === null ? null : new Date(updatedAt) });
      } else {
        await response.body?.cancel();
      }
    } catch (error) {
      // A locked session has nothing more to load. Other failures keep the
      // image already shown and try again at the next check.
      if (error instanceof AuthenticationRequired) return;
    }
    if (!current()) return;
    const now = this.#now();
    entry.checked = true;
    entry.checkedOffline = !online;
    entry.nextCheckAt = nextCheckAt(now, updatedAt);
  }

  // Forgets devices that left the inventory.
  retain(ids: ReadonlySet<string>) {
    for (const [id, entry] of this.#entries) {
      if (ids.has(id) || entry.listeners.size) continue;
      if (entry.thumbnail) this.#revokeUrl(entry.thumbnail.url);
      this.#entries.delete(id);
    }
  }

  // Discards every image, for sign-out and the session lock.
  clear() {
    this.#generation++;
    for (const entry of this.#entries.values()) {
      entry.etag = null;
      entry.checked = false;
      entry.checkedOffline = false;
      entry.nextCheckAt = 0;
      if (entry.thumbnail) this.#publish(entry, null);
    }
  }
}
