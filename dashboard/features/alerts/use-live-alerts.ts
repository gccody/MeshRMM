import { useCallback, useEffect, useRef, useState } from "react";
import { AuthenticationRequired, errorText } from "../../lib/http";
import { type ReloadGate, reloadGate } from "../../lib/reload-gate";
import { frameScheduler } from "../agents/device-store";
import { useWorkspace } from "../workspace/workspace-context";
import { fetchAlerts } from "./alerts-api";
import { ALERT_PAGE_LIMIT, type Alert, type AlertFilters, type AlertList, applyAlertChanges, needsRefill, parseAlertFilters } from "./model";
import { useAlertEvents } from "./use-alert-feed";

// While alerts keep changing in ways the socket only counts, the list is
// read again at most this often.
const RELOAD_INTERVAL_MS = 2_000;

const FAILED = "The alerts could not be loaded.";

type Shown = { data: AlertList | null; filters: AlertFilters; error: string | null };

// The first ALERT_PAGE_LIMIT alerts for the filters in `query`, kept
// current by the event socket. Changed alerts are applied a frame's worth at
// a time and the list never grows past the limit. The socket's counts mean
// changes it didn't send one by one, so they read the list again, but at
// most once every RELOAD_INTERVAL_MS, never two reads at once, and not
// while the tab is hidden. Only the newest read's answer is shown, with the
// changes that arrived while it was on its way.
export function useLiveAlerts(query: string) {
  const { authorizedFetch } = useWorkspace();
  const [shown, setShown] = useState<Shown>(() => ({ data: null, filters: parseAlertFilters(new URLSearchParams(query)), error: null }));
  const tickets = useRef(0);
  // Changes since the newest read was sent, and those not yet shown.
  const sinceRead = useRef(new Map<string, Alert>());
  const queued = useRef(new Map<string, Alert>());
  const flushing = useRef(false);
  const gate = useRef<ReloadGate | null>(null);

  const read = useCallback(async () => {
    const ticket = ++tickets.current;
    sinceRead.current = new Map();
    const filters = parseAlertFilters(new URLSearchParams(query));
    const params = new URLSearchParams(query);
    params.set("limit", String(ALERT_PAGE_LIMIT));
    try {
      const list = await fetchAlerts(authorizedFetch, params.toString());
      if (ticket !== tickets.current) return;
      const data = applyAlertChanges(list, sinceRead.current.values(), filters, ALERT_PAGE_LIMIT);
      setShown({ data, filters, error: null });
    } catch (failure) {
      if (ticket !== tickets.current || failure instanceof AuthenticationRequired) return;
      setShown((current) => ({ ...current, error: errorText(failure, FAILED) }));
    }
  }, [authorizedFetch, query]);

  const latestRead = useRef(read);
  useEffect(() => {
    latestRead.current = read;
  }, [read]);

  useEffect(() => {
    void read();
  }, [read]);

  useEffect(() => {
    const current = reloadGate({
      reload: () => latestRead.current(),
      intervalMs: RELOAD_INTERVAL_MS,
      hidden: () => document.visibilityState === "hidden",
    });
    const handleVisibility = () => {
      if (document.visibilityState === "visible") current.shown();
    };
    document.addEventListener("visibilitychange", handleVisibility);
    gate.current = current;
    return () => {
      document.removeEventListener("visibilitychange", handleVisibility);
      current.stop();
      gate.current = null;
    };
  }, []);

  // A changed alert, shown with the others that arrive before the next frame.
  const apply = useCallback((alert: Alert) => {
    sinceRead.current.set(alert.id, alert);
    queued.current.set(alert.id, alert);
    if (flushing.current) return;
    flushing.current = true;
    frameScheduler(() => {
      flushing.current = false;
      const batch = [...queued.current.values()];
      queued.current = new Map();
      setShown((current) => (current.data
        ? { ...current, data: applyAlertChanges(current.data, batch, current.filters, ALERT_PAGE_LIMIT) }
        : current));
    });
  }, []);

  useAlertEvents((event) => {
    if (event.type === "alert_upsert") apply(event.alert);
    else if (event.type === "alert_counts") gate.current?.request();
  });

  // Alerts left a list that had more: read it again to fill it.
  const { data } = shown;
  useEffect(() => {
    if (data && needsRefill(data, ALERT_PAGE_LIMIT)) gate.current?.request();
  }, [data]);

  const reload = useCallback(() => {
    void read();
  }, [read]);

  return { data, error: shown.error, reload, apply };
}
