import { useCallback, useEffect, useEffectEvent, useState } from "react";
import type { AuthorizedFetch } from "../../lib/http";
import { useWorkspace } from "../workspace/workspace-context";
import type { AlertCounts, AlertEvent } from "./model";

export type AlertFeed = {
  // How many alerts are unresolved, once known.
  counts: AlertCounts | null;
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
  const [counts, setCounts] = useState<AlertCounts | null>(null);
  const [listeners] = useState(() => new Set<(event: AlertEvent) => void>());

  const handle = useCallback((event: AlertEvent) => {
    setCounts(event.counts);
    for (const listener of listeners) listener(event);
  }, [listeners]);

  const refreshCounts = useCallback(async () => {
    if (!enabled) return;
    const next = await readCounts(authorizedFetch);
    if (next) setCounts(next);
  }, [authorizedFetch, enabled]);

  useEffect(() => {
    if (!enabled) return;
    let cancelled = false;
    void readCounts(authorizedFetch).then((next) => {
      if (!cancelled && next) setCounts(next);
    });
    return () => {
      cancelled = true;
    };
  }, [authorizedFetch, enabled]);

  const subscribe = useCallback((listener: (event: AlertEvent) => void) => {
    listeners.add(listener);
    return () => {
      listeners.delete(listener);
    };
  }, [listeners]);

  return { counts: enabled ? counts : null, handle, subscribe, refreshCounts };
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

// Calls `listener` with each alert event while the page is open.
export function useAlertEvents(listener: (event: AlertEvent) => void) {
  const { alerts } = useWorkspace();
  const onEvent = useEffectEvent(listener);
  const { subscribe } = alerts;
  useEffect(() => subscribe((event) => onEvent(event)), [subscribe]);
}
