import { type RefObject, useCallback, useEffect, useRef, useState } from "react";
import type { AlertEvent } from "../alerts/model";
import type { JobEvent } from "../automation/model";
import type { MetricsReading } from "../metrics/model";
import type { UpdateEvent } from "../updates/model";
import { type InventoryConnection, STALE_GRACE_MS, eventsSocketUrl, inventoryStream } from "./inventory-stream";
import type { Inventory } from "./types";

type Options = {
  enabled: boolean;
  // Shared with the HTTP inventory load so neither applies older data.
  revisionRef: RefObject<number>;
  onInventory: (update: (current: Inventory) => Inventory) => void;
  onMetrics: (readings: MetricsReading[]) => void;
  onAlerts: (event: AlertEvent) => void;
  onJobs: (event: JobEvent) => void;
  onUpdates: (event: UpdateEvent) => void;
  onRefused: () => void;
};

// `since` is when the stream last stopped being live; null before it starts.
type Link = { connection: InventoryConnection; since: number | null };

// The live inventory socket, and how long it has been down.
export function useInventoryConnection({ enabled, revisionRef, onInventory, onMetrics, onAlerts, onJobs, onUpdates, onRefused }: Options) {
  const [link, setLink] = useState<Link>({ connection: "connecting", since: null });
  // Advanced by a timer when the stale grace period ends.
  const [clock, setClock] = useState(0);
  const stream = useRef<ReturnType<typeof inventoryStream> | null>(null);
  const refused = useRef(onRefused);
  useEffect(() => { refused.current = onRefused; }, [onRefused]);
  const alerts = useRef(onAlerts);
  useEffect(() => { alerts.current = onAlerts; }, [onAlerts]);
  const jobs = useRef(onJobs);
  useEffect(() => { jobs.current = onJobs; }, [onJobs]);
  const updates = useRef(onUpdates);
  useEffect(() => { updates.current = onUpdates; }, [onUpdates]);

  useEffect(() => {
    if (!enabled) return;
    revisionRef.current = -1;
    const current = inventoryStream({
      openSocket: () => new WebSocket(eventsSocketUrl(window.location)),
      revision: revisionRef,
      onInventory,
      onMetrics,
      onAlerts: (event) => alerts.current(event),
      onJobs: (event) => jobs.current(event),
      onUpdates: (event) => updates.current(event),
      onConnection: (connection) => {
        const at = Date.now();
        setLink((previous) => {
          if (previous.connection === connection && previous.since !== null) return previous;
          const since = previous.connection === "live" || previous.since === null ? at : previous.since;
          return { connection, since };
        });
      },
      onRefused: () => refused.current(),
      online: navigator.onLine,
    });
    stream.current = current;
    return () => {
      current.stop();
      stream.current = null;
    };
  }, [enabled, onInventory, onMetrics, revisionRef]);

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

  // Reconnects now, skipping the backoff wait.
  const reconnect = useCallback(() => stream.current?.wake(), []);

  return { ...link, now: clock, reconnect };
}
