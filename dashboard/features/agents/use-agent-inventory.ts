import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { AuthenticationRequired, type AuthorizedFetch, errorMessage } from "../../lib/http";
import type { AlertEvent } from "../alerts/model";
import type { JobEvent } from "../automation/model";
import type { MetricsReading } from "../metrics/model";
import type { UpdateEvent } from "../updates/model";
import { DeviceStore } from "./device-store";
import { type InventoryTarget, inventoryStatus, refreshInventory } from "./inventory-stream";
import { parseAgentList } from "./model";
import { ThumbnailStore } from "./thumbnails";
import { useDevicesLoaded } from "./use-devices";
import { useInventoryConnection } from "./use-inventory-connection";

type Options = {
  enabled: boolean;
  authorizedFetch: AuthorizedFetch;
  // The socket was refused or lost access; the session needs checking.
  onRefused: () => void;
  // What the socket says about alerts.
  onAlerts: (event: AlertEvent) => void;
  // What the socket says about jobs.
  onJobs: (event: JobEvent) => void;
  // What the socket says about updates.
  onUpdates: (event: UpdateEvent) => void;
};

async function fetchAgentList(authorizedFetch: AuthorizedFetch) {
  const response = await authorizedFetch("/v1/agents");
  if (!response.ok) throw new Error(await errorMessage(response, "The devices could not be loaded."));
  const data = parseAgentList(await response.json());
  if (!data) throw new Error("The server returned an invalid device list.");
  return data;
}

// The live inventory: the devices, companies, tags and readings in a store
// that components subscribe to piece by piece (see use-devices.ts), and the
// state of the connection that keeps it current. What it returns changes
// only with that state, not with the devices.
export function useAgentInventory({ enabled, authorizedFetch, onRefused, onAlerts, onJobs, onUpdates }: Options) {
  const [devices] = useState(() => new DeviceStore());
  const hasData = useDevicesLoaded(devices);
  const [isRefreshing, setIsRefreshing] = useState(false);
  const [error, setError] = useState<string | null>(null);
  // Whether `error` is set, so the stream's changes clear it without a
  // state update each.
  const failed = useRef(false);
  const revision = useRef(-1);
  // Screen images outlive page changes with the inventory they belong to.
  const [thumbnails] = useState(() => new ThumbnailStore({ fetch: authorizedFetch }));
  useEffect(() => thumbnails.setFetch(authorizedFetch), [authorizedFetch, thumbnails]);
  // A device that left takes its image with it.
  useEffect(() => devices.subscribe("ids", () => thumbnails.retain(devices)), [devices, thumbnails]);

  const fail = useCallback((message: string | null) => {
    if (!failed.current && message === null) return;
    failed.current = message !== null;
    setError(message);
  }, []);

  // Only sign-out and the session lock discard the inventory.
  const reset = useCallback(() => {
    thumbnails.clear();
    devices.clear();
    fail(null);
    revision.current = -1;
  }, [devices, fail, thumbnails]);

  // A user who may no longer see devices sees none.
  useEffect(() => {
    if (!enabled) devices.clear();
  }, [devices, enabled]);

  const loadAgents = useCallback(async () => {
    if (!enabled) return false;
    setIsRefreshing(true);
    try {
      const data = await fetchAgentList(authorizedFetch);
      fail(null);
      // The open socket already applied newer data. While it is down the
      // revision is -1, so a restarted server's list applies.
      if (data.revision < revision.current) return true;
      revision.current = data.revision;
      devices.replace(data);
      return true;
    } catch (requestError) {
      if (requestError instanceof AuthenticationRequired) return false;
      // Keep the devices already on screen; the stale state explains them.
      fail(`Couldn’t refresh devices: ${requestError instanceof Error ? requestError.message : "the server could not be reached."}`);
      return false;
    } finally {
      setIsRefreshing(false);
    }
  }, [authorizedFetch, devices, enabled, fail]);

  const target = useMemo<InventoryTarget>(() => ({
    replace: (inventory) => {
      devices.replace(inventory);
      fail(null);
    },
    apply: (change) => {
      devices.apply(change);
      fail(null);
    },
  }), [devices, fail]);
  const applyMetrics = useCallback((readings: MetricsReading[]) => devices.applyReadings(readings), [devices]);
  const { connection, since, now, reconnect } = useInventoryConnection({ enabled, revisionRef: revision, inventory: target, onMetrics: applyMetrics, onAlerts, onJobs, onUpdates, onRefused });

  const refresh = useCallback(
    () => refreshInventory(connection, { wake: reconnect, load: loadAgents }),
    [connection, loadAgents, reconnect],
  );

  const status = enabled ? inventoryStatus({ hasData, connection, since, now }) : "loading";
  const shownError = enabled ? error : null;
  return useMemo(() => ({
    devices,
    thumbnails,
    hasData: enabled && hasData,
    status,
    connection,
    error: shownError,
    isRefreshing,
    loadAgents,
    refresh,
    reconnect,
    reset,
  }), [connection, devices, enabled, hasData, isRefreshing, loadAgents, reconnect, refresh, reset, shownError, status, thumbnails]);
}
