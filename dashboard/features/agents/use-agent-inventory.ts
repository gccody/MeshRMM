import { useCallback, useEffect, useRef, useState } from "react";
import { AuthenticationRequired, type AuthorizedFetch, errorMessage } from "../../lib/http";
import { inventoryStatus } from "./inventory-stream";
import { parseAgentList, sortAgents } from "./model";
import { ThumbnailStore } from "./thumbnails";
import type { Agent } from "./types";
import { useInventoryConnection } from "./use-inventory-connection";

type Options = {
  enabled: boolean;
  authorizedFetch: AuthorizedFetch;
  // The socket was refused or lost access; the session needs checking.
  onRefused: () => void;
};

async function fetchAgentList(authorizedFetch: AuthorizedFetch) {
  const response = await authorizedFetch("/v1/agents");
  if (!response.ok) throw new Error(await errorMessage(response, "The devices could not be loaded."));
  const data = parseAgentList(await response.json());
  if (!data) throw new Error("The server returned an invalid device list.");
  return data;
}

export function useAgentInventory({ enabled, authorizedFetch, onRefused }: Options) {
  const [agents, setAgents] = useState<Agent[]>([]);
  const [hasData, setHasData] = useState(false);
  const [isRefreshing, setIsRefreshing] = useState(false);
  const [lastUpdated, setLastUpdated] = useState<Date | null>(null);
  const [error, setError] = useState<string | null>(null);
  const revision = useRef(-1);
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

  const loadAgents = useCallback(async () => {
    if (!enabled) return false;
    setIsRefreshing(true);
    try {
      const data = await fetchAgentList(authorizedFetch);
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
      setError(`Couldn’t refresh devices: ${requestError instanceof Error ? requestError.message : "the server could not be reached."}`);
      return false;
    } finally {
      setIsRefreshing(false);
    }
  }, [authorizedFetch, enabled]);

  const applyStreamUpdate = useCallback((update: (current: Agent[]) => Agent[]) => {
    setAgents(update);
    setHasData(true);
    setLastUpdated(new Date());
    setError(null);
  }, []);
  const { connection, since, now, reconnect } = useInventoryConnection({ enabled, revisionRef: revision, onAgents: applyStreamUpdate, onRefused });

  // Refresh reloads the list and, while the stream is down, skips the
  // reconnect wait. An open stream is already current.
  const isStreamLive = connection === "live";
  const refresh = useCallback(() => {
    if (!isStreamLive) reconnect();
    return loadAgents();
  }, [isStreamLive, loadAgents, reconnect]);

  const status = inventoryStatus({ hasData, connection, since, now });
  return {
    agents: enabled ? agents : [],
    hasData: enabled && hasData,
    status: enabled ? status : "loading",
    connection,
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
