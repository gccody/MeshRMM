import { AuthenticationRequired, type AuthorizedFetch } from "../../lib/http.ts";

// The Agent uploads an image of its main display this often.
export const THUMBNAIL_INTERVAL_MS = 5 * 60_000;
// Checking this long after the Agent's next upload is due finds it.
const UPLOAD_GRACE_MS = 20_000;
// Never check one device more often than this.
const MIN_CHECK_DELAY_MS = 10_000;
// Images of this many devices no tile shows are kept, the most recently
// shown, so scrolling back draws them at once. An image fits 640 by 400 and
// is a few dozen KiB as a JPEG, so this is a few MiB, and at most 60 MiB at
// the server's 512 KiB limit. The wall draws about 50 tiles on a large
// screen, so this is two or three screens back.
export const RECENT_THUMBNAILS = 120;
// At most this many images load at once. Browsers open six connections to a
// server over HTTP/1.1; this leaves two for the rest of the website.
export const THUMBNAIL_REQUESTS = 4;

export type Thumbnail = {
  url: string;
  // When the server received the image.
  updatedAt: Date | null;
};

type Load = {
  done: Promise<void>;
  controller: AbortController;
};

type Entry = {
  thumbnail: Thumbnail | null;
  etag: string | null;
  checked: boolean;
  // Checked while the device was offline; it uploads again when it connects.
  checkedOffline: boolean;
  nextCheckAt: number;
  load: Load | null;
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
// downloaded again. Only visible rows ask for them: a device no row shows
// any more stops loading, and only the most recently shown keep their image.
export class ThumbnailStore {
  #entries = new Map<string, Entry>();
  // The devices no row shows, the longest hidden first.
  #hidden = new Set<string>();
  // Loads waiting for a request to finish, in the order they asked.
  #waiting = new Set<() => void>();
  #active = 0;
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
      entry = { thumbnail: null, etag: null, checked: false, checkedOffline: false, nextCheckAt: 0, load: null, listeners: new Set() };
      this.#entries.set(id, entry);
      this.#hide(id);
    }
    return entry;
  }

  #publish(entry: Entry, thumbnail: Thumbnail | null) {
    if (entry.thumbnail) this.#revokeUrl(entry.thumbnail.url);
    entry.thumbnail = thumbnail;
    for (const listener of entry.listeners) listener();
  }

  // Marks a device as shown by no row, and forgets the longest hidden past
  // the limit.
  #hide(id: string) {
    this.#hidden.delete(id);
    this.#hidden.add(id);
    for (const hidden of this.#hidden) {
      if (this.#hidden.size <= RECENT_THUMBNAILS) break;
      this.#forget(hidden);
    }
  }

  // A forgotten device starts over when a row shows it again. Its ETag goes
  // too: the server would answer it with no image.
  #forget(id: string) {
    const entry = this.#entries.get(id);
    this.#hidden.delete(id);
    if (!entry) return;
    this.#abort(entry);
    if (entry.thumbnail) this.#revokeUrl(entry.thumbnail.url);
    this.#entries.delete(id);
  }

  // Stops the device's load; a response that still comes is dropped.
  #abort(entry: Entry) {
    entry.load?.controller.abort();
    entry.load = null;
  }

  // Uses the website's current request function from now on.
  setFetch(fetch: AuthorizedFetch) {
    this.#fetch = fetch;
  }

  snapshot(id: string): Thumbnail | null {
    return this.#entries.get(id)?.thumbnail ?? null;
  }

  // A row showing the device. When the last one stops, so does its load.
  subscribe(id: string, listener: () => void) {
    const entry = this.#entry(id);
    entry.listeners.add(listener);
    this.#hidden.delete(id);
    return () => {
      if (!entry.listeners.delete(listener) || entry.listeners.size || this.#entries.get(id) !== entry) return;
      this.#abort(entry);
      this.#hide(id);
    };
  }

  // Loads the device's image if a check is due, and returns how long until
  // the next one, or null when there is none: an offline device uploads
  // nothing, so its last image is loaded once, and a locked or cleared
  // website, or a device no row shows any more, loads nothing more.
  async refresh(id: string, online: boolean): Promise<number | null> {
    const entry = this.#entry(id);
    if (online && entry.checkedOffline) {
      entry.checkedOffline = false;
      entry.nextCheckAt = Math.min(entry.nextCheckAt, this.#now() + UPLOAD_GRACE_MS);
    }
    if (!entry.load && (!entry.checked || (online && this.#now() >= entry.nextCheckAt))) this.#start(id, entry, online);
    if (entry.load) await entry.load.done;
    if (!online || !entry.checked || this.#entries.get(id) !== entry) return null;
    return Math.max(0, entry.nextCheckAt - this.#now());
  }

  #start(id: string, entry: Entry, online: boolean) {
    const controller = new AbortController();
    const load: Load = { controller, done: Promise.resolve() };
    load.done = this.#load(id, entry, online, controller.signal).finally(() => {
      if (entry.load === load) entry.load = null;
    });
    entry.load = load;
  }

  // Waits for a free request, and returns the function that gives it back,
  // or null if the load was stopped while it waited.
  #request(signal: AbortSignal): Promise<(() => void) | null> {
    return new Promise((resolve) => {
      if (signal.aborted) return resolve(null);
      const stop = () => {
        this.#waiting.delete(start);
        resolve(null);
      };
      const start = () => {
        signal.removeEventListener("abort", stop);
        this.#active++;
        let released = false;
        resolve(() => {
          if (released) return;
          released = true;
          this.#active--;
          for (const next of this.#waiting) {
            if (this.#active >= THUMBNAIL_REQUESTS) break;
            this.#waiting.delete(next);
            next();
          }
        });
      };
      if (this.#active < THUMBNAIL_REQUESTS) return start();
      this.#waiting.add(start);
      signal.addEventListener("abort", stop, { once: true });
    });
  }

  async #load(id: string, entry: Entry, online: boolean, signal: AbortSignal) {
    const generation = this.#generation;
    const current = () => !signal.aborted && generation === this.#generation && this.#entries.get(id) === entry;
    const release = await this.#request(signal);
    if (!release) return;
    let updatedAt: number | null = null;
    try {
      const response = await this.#fetch(`/v1/agents/${encodeURIComponent(id)}/thumbnail`, {
        cache: "no-store",
        headers: entry.etag ? { "If-None-Match": entry.etag } : {},
        signal,
      });
      if (!current()) {
        await response.body?.cancel();
        return;
      }
      if (response.status === 304) {
        updatedAt = entry.thumbnail?.updatedAt?.getTime() ?? null;
      } else if (response.status === 204) {
        entry.etag = null;
        if (entry.thumbnail) this.#publish(entry, null);
        await response.body?.cancel();
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
    } finally {
      release();
    }
    // A stopped load is no check: the device is checked when a row shows
    // it again.
    if (!current()) return;
    const now = this.#now();
    entry.checked = true;
    entry.checkedOffline = !online;
    entry.nextCheckAt = nextCheckAt(now, updatedAt);
  }

  // Forgets devices that left the inventory.
  retain(ids: { has(id: string): boolean }) {
    for (const [id, entry] of this.#entries) {
      if (!ids.has(id) && !entry.listeners.size) this.#forget(id);
    }
  }

  // Discards every image, for sign-out and the session lock.
  clear() {
    this.#generation++;
    for (const entry of this.#entries.values()) {
      this.#abort(entry);
      entry.etag = null;
      entry.checked = false;
      entry.checkedOffline = false;
      entry.nextCheckAt = 0;
      if (entry.thumbnail) this.#publish(entry, null);
    }
  }
}
