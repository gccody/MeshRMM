"use client";

import { useCallback, useEffect, useRef, useState } from "react";
import { errorMessage } from "../../lib/http";
import { AuthenticationRequired, type AuthorizedFetch } from "../../lib/http";
import { type InventoryConnection, STALE_GRACE_MS, inventoryStatus, inventoryStream } from "./inventory-stream";
import { parseAgentList, sortAgents } from "./model";
import { subscriptionRenewal } from "./subscription-renewal";
import { ThumbnailStore } from "./thumbnails";
import type { Agent } from "./types";

type Options = {
  enabled: boolean;
  subscriptionKey?: string;
  authorizedFetch: AuthorizedFetch;
};

// `since` is when the stream last stopped being live; null before it starts.
type Link = { connection: InventoryConnection; since: number | null };

export function useAgentInventory({
  enabled,
  subscriptionKey,
  authorizedFetch,
}: Options) {
  const [agents, setAgents] = useState<Agent[]>([]);
  const [hasData, setHasData] = useState(false);
  const [isRefreshing, setIsRefreshing] = useState(false);
  const [lastUpdated, setLastUpdated] = useState<Date | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [link, setLink] = useState<Link>({ connection: "connecting", since: null });
  // Advanced by a timer when the stale grace period ends.
  const [clock, setClock] = useState(0);
  const revision = useRef(-1);
  const stream = useRef<ReturnType<typeof inventoryStream> | null>(null);
  // Screen images outlive page changes with the inventory they belong to.
  const [thumbnails] = useState(() => new ThumbnailStore({ fetch: authorizedFetch }));
  useEffect(() => thumbnails.setFetch(authorizedFetch), [authorizedFetch, thumbnails]);

  // Only sign-out and the session lock discard the inventory.
  const reset = useCallback(() => {
    thumbnails.clear();
    setAgents([]);
    setHasData(false);
    setLastUpdated(null);
    setError(null);
    revision.current = -1;
  }, [thumbnails]);

  useEffect(() => {
    if (hasData) thumbnails.retain(new Set(agents.map((agent) => agent.id)));
  }, [agents, hasData, thumbnails]);

  const loadAgents = useCallback(
    async (silent = false) => {
      if (!enabled) return false;
      if (!silent) setIsRefreshing(true);
      try {
        const response = await authorizedFetch("/v1/agents");
        if (!response.ok) {
          throw new Error(
            await errorMessage(response, "The live agent service could not be reached."),
          );
        }
        const data = parseAgentList(await response.json());
        if (!data) throw new Error("The live agent service returned an invalid response.");
        setError(null);
        if (data.revision < revision.current) return true;
        revision.current = data.revision;
        setAgents(sortAgents(data.agents));
        setHasData(true);
        setLastUpdated(new Date());
        return true;
      } catch (requestError) {
        if (requestError instanceof AuthenticationRequired) return false;
        // Keep the devices already on screen; the stale state explains them.
        setError(`Couldn’t refresh devices: ${
          requestError instanceof Error
            ? requestError.message
            : "The live agent service could not be reached."
        }`);
        return false;
      } finally {
        setIsRefreshing(false);
      }
    },
    [authorizedFetch, enabled],
  );

  useEffect(() => {
    if (!enabled || !subscriptionKey) return;
    revision.current = -1;
    const current = inventoryStream({
      subscribe: async () => {
        try {
          return await authorizedFetch("/v1/agents/events/subscriptions", { method: "POST" });
        } catch (requestError) {
          if (requestError instanceof AuthenticationRequired) return null;
          throw requestError;
        }
      },
      openSocket: (url) => new WebSocket(url),
      renewal: (disconnect) => subscriptionRenewal(authorizedFetch, disconnect),
      revision,
      onAgents: (update) => {
        setAgents(update);
        setHasData(true);
        setLastUpdated(new Date());
        setError(null);
      },
      onConnection: (connection) => {
        const at = Date.now();
        setLink((previous) => {
          if (previous.connection === connection && previous.since !== null) return previous;
          const since = previous.connection === "live" || previous.since === null ? at : previous.since;
          return { connection, since };
        });
      },
      onError: setError,
      online: navigator.onLine,
    });
    stream.current = current;
    return () => {
      current.stop();
      stream.current = null;
    };
  }, [authorizedFetch, enabled, subscriptionKey]);

  // Resynchronize when the network returns or the tab becomes visible again,
  // for example after the computer wakes from sleep.
  useEffect(() => {
    if (!enabled) return;
    const handleOnline = () => {
      stream.current?.setOnline(true);
      stream.current?.wake();
    };
    const handleOffline = () => stream.current?.setOnline(false);
    const handleVisibility = () => {
      if (document.visibilityState === "visible") stream.current?.wake();
    };
    window.addEventListener("online", handleOnline);
    window.addEventListener("offline", handleOffline);
    document.addEventListener("visibilitychange", handleVisibility);
    return () => {
      window.removeEventListener("online", handleOnline);
      window.removeEventListener("offline", handleOffline);
      document.removeEventListener("visibilitychange", handleVisibility);
    };
  }, [enabled]);

  useEffect(() => {
    if (link.connection === "live" || link.since === null) return;
    const timer = window.setTimeout(
      () => setClock(Date.now()),
      Math.max(0, link.since + STALE_GRACE_MS - Date.now()),
    );
    return () => window.clearTimeout(timer);
  }, [link]);

  // Reconnects now (also after the server refused the subscription).
  const reconnect = useCallback(() => stream.current?.wake(), []);

  // Refresh reloads the list and, while the stream is down, skips the
  // reconnect wait. An open stream is already current.
  const isStreamLive = link.connection === "live";
  const refresh = useCallback(() => {
    if (!isStreamLive) stream.current?.wake();
    return loadAgents();
  }, [isStreamLive, loadAgents]);

  const status = inventoryStatus({ hasData, connection: link.connection, since: link.since, now: clock });
  return {
    agents: enabled ? agents : [],
    hasData: enabled && hasData,
    status: enabled ? status : "loading",
    connection: link.connection,
    lastUpdated,
    error: enabled ? error : null,
    isRefreshing,
    loadAgents,
    refresh,
    reconnect,
    reset,
    thumbnails,
  };
}
