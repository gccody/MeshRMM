import { AuthenticationRequired, type AuthorizedFetch } from "../../lib/http.ts";

// Images of this many devices no tile shows are kept, the most recently
// shown, so scrolling back draws them at once. An image fits 640 by 400 and
// is a few dozen KiB as a JPEG, so this is a few MiB, and at most 60 MiB at
// the server's 512 KiB limit. The wall draws about 50 tiles on a large
// screen, so this is two or three screens back.
export const RECENT_THUMBNAILS = 120;
// At most this many images load at once. Browsers open six connections to a
// server over HTTP/1.1; this leaves two for the rest of the website.
export const THUMBNAIL_REQUESTS = 4;
// A load that failed is tried again this long after.
export const RETRY_DELAY_MS = 60_000;

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
  // Loaded since the image last changed, as far as this page knows.
  checked: boolean;
  // The image changed while a load was running, so load it again after.
  stale: boolean;
  // The last load failed, and may work if tried again.
  failed: boolean;
  // Tries it again, while a row shows the device.
  retry: ReturnType<typeof setTimeout> | undefined;
  load: Load | null;
  listeners: Set<() => void>;
};

type Options = {
  fetch: AuthorizedFetch;
  createUrl?: (blob: Blob) => string;
  revokeUrl?: (url: string) => void;
  // Whether the page is in front.
  visible?: () => boolean;
};

// The latest screen image of each device, shared by every row that shows it.
// A row shown loads its device's image once. The Agent of a device a tab
// shows refreshes it within seconds and every few minutes, and the event
// socket says when it did (`replaced`), so it is loaded again then, not
// polled. What the socket may not have said, for a device it was not
// watching, as before it reconnected or while the tab was hidden, is
// checked when it starts watching it (`revalidate`). Images are revalidated
// with their ETag, so an unchanged one is not downloaded again. A load that
// failed, whoever started it, is tried again a while later while a row
// shows the device and the page is in front. Only visible rows ask for
// them: a device no row shows any more stops loading, and only the most
// recently shown keep their image.
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
  #visible: () => boolean;

  constructor({ fetch, createUrl = (blob) => URL.createObjectURL(blob), revokeUrl = (url) => URL.revokeObjectURL(url), visible = () => document.visibilityState === "visible" }: Options) {
    this.#fetch = fetch;
    this.#createUrl = createUrl;
    this.#revokeUrl = revokeUrl;
    this.#visible = visible;
  }

  #entry(id: string) {
    let entry = this.#entries.get(id);
    if (!entry) {
      entry = { thumbnail: null, etag: null, checked: false, stale: false, failed: false, retry: undefined, load: null, listeners: new Set() };
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

  // Stops the device's load, and trying a failed one again; a response
  // that still comes is dropped.
  #abort(entry: Entry) {
    clearTimeout(entry.retry);
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

  // Loads the device's image unless this page has it as it is now. A
  // locked or cleared website, or a device no row shows any more, loads
  // nothing.
  async refresh(id: string) {
    const entry = this.#entry(id);
    if (!entry.load && !entry.checked) this.#start(id, entry);
    await entry.load?.done;
  }

  // The server replaced these devices' images: the rows showing one load
  // it now, after any load still running, and the others when next shown.
  replaced(ids: Iterable<string>) {
    for (const id of ids) {
      const entry = this.#entries.get(id);
      if (!entry) continue;
      entry.checked = false;
      if (entry.load) entry.stale = true;
      else if (entry.listeners.size) this.#start(id, entry);
    }
  }

  // The event socket started watching these devices, so it may have missed
  // their new images. Those this page loaded are checked again; any other
  // loads when shown anyway.
  revalidate(ids: Iterable<string>) {
    this.replaced([...ids].filter((id) => this.#entries.get(id)?.checked));
  }

  #start(id: string, entry: Entry) {
    const controller = new AbortController();
    const load: Load = { controller, done: Promise.resolve() };
    clearTimeout(entry.retry);
    entry.stale = false;
    entry.failed = false;
    load.done = this.#load(id, entry, controller.signal).finally(() => {
      if (entry.load !== load) return;
      entry.load = null;
      if (entry.stale) this.replaced([id]);
      else if (entry.failed && entry.listeners.size) entry.retry = setTimeout(() => this.#retry(id, entry), RETRY_DELAY_MS);
    });
    entry.load = load;
  }

  // Tries a failed load again. A page not in front waits: its rows check
  // when it is shown.
  #retry(id: string, entry: Entry) {
    if (this.#entries.get(id) === entry && !entry.load && !entry.checked && this.#visible()) this.#start(id, entry);
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

  async #load(id: string, entry: Entry, signal: AbortSignal) {
    const generation = this.#generation;
    const current = () => !signal.aborted && generation === this.#generation && this.#entries.get(id) === entry;
    const release = await this.#request(signal);
    if (!release) return;
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
      entry.failed = !response.ok && response.status !== 304;
      if (response.status === 204) {
        entry.etag = null;
        if (entry.thumbnail) this.#publish(entry, null);
        await response.body?.cancel();
      } else if (response.ok) {
        const blob = await response.blob();
        if (!current()) return;
        const modified = Date.parse(response.headers.get("Last-Modified") ?? "");
        entry.etag = response.headers.get("ETag");
        this.#publish(entry, { url: this.#createUrl(blob), updatedAt: Number.isNaN(modified) ? null : new Date(modified) });
      } else {
        await response.body?.cancel();
      }
    } catch (error) {
      // A locked session has nothing more to load. Other failures keep the
      // image already shown, and are tried again.
      entry.failed = current() && !(error instanceof AuthenticationRequired);
      return;
    } finally {
      release();
    }
    // A stopped load is no check: the device is checked when a row shows
    // it again.
    if (current() && !entry.failed && !entry.stale) entry.checked = true;
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
      entry.stale = false;
      entry.failed = false;
      if (entry.thumbnail) this.#publish(entry, null);
    }
  }
}
