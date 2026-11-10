import { useCallback, useEffect, useEffectEvent, useMemo, useState, useSyncExternalStore } from "react";
import type { AuthorizedFetch } from "../../lib/http";
import { useWorkspace } from "../workspace/workspace-context";
import type { AlertCounts, AlertEvent } from "./model";

// The unresolved alert counts, held outside React so that an alert event
// re-renders only what shows them.
class CountsStore {
  #counts: AlertCounts | null = null;
  #listeners = new Set<() => void>();

  get = () => this.#counts;

  set(counts: AlertCounts | null) {
    if (counts === this.#counts) return;
    this.#counts = counts;
    for (const listener of [...this.#listeners]) listener();
  }

  subscribe = (listener: () => void) => {
    this.#listeners.add(listener);
    return () => {
      this.#listeners.delete(listener);
    };
  };
}

export type AlertFeed = {
  // How many alerts are unresolved, once known; read with useAlertCounts.
  counts: CountsStore;
  // Hands the socket's alert events to the shell's listeners.
  handle: (event: AlertEvent) => void;
  // Calls `listener` with each alert event until the returned function runs.
  subscribe: (listener: (event: AlertEvent) => void) => () => void;
  // Reads the counts again, after a change made while the socket is down.
  refreshCounts: () => Promise<void>;
};

// The unresolved alert counts the navigation shows, kept current by the
// event socket, and its alert events for the page beneath. Users who can't
// open the socket (it needs devices.view) get the counts when they load.
export function useAlertFeed({ enabled, authorizedFetch }: { enabled: boolean; authorizedFetch: AuthorizedFetch }): AlertFeed {
  const [counts] = useState(() => new CountsStore());
  const [listeners] = useState(() => new Set<(event: AlertEvent) => void>());

  const handle = useCallback((event: AlertEvent) => {
    if (event.type !== "monitor_update") counts.set(event.counts);
    for (const listener of listeners) listener(event);
  }, [counts, listeners]);

  const refreshCounts = useCallback(async () => {
    if (!enabled) return;
    const next = await readCounts(authorizedFetch);
    if (next) counts.set(next);
  }, [authorizedFetch, counts, enabled]);

  useEffect(() => {
    if (!enabled) {
      counts.set(null);
      return;
    }
    let cancelled = false;
    void readCounts(authorizedFetch).then((next) => {
      if (!cancelled && next) counts.set(next);
    });
    return () => {
      cancelled = true;
    };
  }, [authorizedFetch, counts, enabled]);

  const subscribe = useCallback((listener: (event: AlertEvent) => void) => {
    listeners.add(listener);
    return () => {
      listeners.delete(listener);
    };
  }, [listeners]);

  return useMemo(() => ({ counts, handle, subscribe, refreshCounts }), [counts, handle, subscribe, refreshCounts]);
}

// The counts, or null if they couldn't be read; the next event brings new
// ones.
async function readCounts(authorizedFetch: AuthorizedFetch) {
  try {
    const response = await authorizedFetch("/v1/alerts/counts");
    if (response.ok) return (await response.json()) as AlertCounts;
    await response.body?.cancel();
  } catch {
    // As if unread.
  }
  return null;
}

// The unresolved alert counts, once known.
export function useAlertCounts(feed: AlertFeed) {
  return useSyncExternalStore(feed.counts.subscribe, feed.counts.get, feed.counts.get);
}

// Calls `listener` with each alert event while the page is open.
export function useAlertEvents(listener: (event: AlertEvent) => void) {
  const { alerts } = useWorkspace();
  const onEvent = useEffectEvent(listener);
  const { subscribe } = alerts;
  useEffect(() => subscribe((event) => onEvent(event)), [subscribe]);
}
