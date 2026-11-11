import { CalendarClock, LoaderCircle, Wrench } from "lucide-react";
import { useCallback, useRef, useState } from "react";
import { AuthenticationRequired, errorText } from "../../lib/http";
import { formatDateTime, timeFormat } from "../../lib/format";
import { useNow } from "../../lib/use-now";
import { useResource } from "../../lib/use-resource";
import type { Agent, CurrentMaintenance } from "../agents/types";
import { useWorkspace } from "../workspace/workspace-context";
import { endMaintenance, endWindow, fetchDeviceWindows } from "./maintenance-api";
import { StartMaintenanceDialog } from "./maintenance-dialog";
import { holdsBack, maintenanceLine, untilText, windowSetBy } from "./model";

const time = timeFormat({ hour: "numeric", minute: "2-digit" });
export const MAINTENANCE_FORMAT = { time, dateTime: formatDateTime };

// The device's maintenance in force, in a line, wherever people act on the
// device by hand: its page, its terminals and its files.
export function MaintenanceNotice({ current }: { current: CurrentMaintenance }) {
  const now = useNow(60_000);
  return (
    <p className="maintenance-notice" role="status">
      <Wrench size={15} aria-hidden="true" />
      <span>{maintenanceLine(current, now, MAINTENANCE_FORMAT)}</span>
    </p>
  );
}

// The device page's maintenance: a banner while it is in maintenance, with
// who set it, why, until when and a way to end it; its windows to come; and
// a button to put it in maintenance.
export function DeviceMaintenance({ agent }: { agent: Agent }) {
  const { authorizedFetch, can } = useWorkspace();
  const canChange = can("devices.maintenance");
  const now = useNow(30_000);
  const [starting, setStarting] = useState(false);
  const [busy, setBusy] = useState<string | null>(null);
  const [error, setError] = useState<string | null>(null);
  const opener = useRef<HTMLButtonElement>(null);
  // Presence tells of every change; the windows are read again for it.
  const changeKey = JSON.stringify(agent.maintenance ?? null);
  const load = useCallback(() => {
    void changeKey;
    return fetchDeviceWindows(authorizedFetch, agent.id);
  }, [authorizedFetch, agent.id, changeKey]);
  const { data, reload } = useResource(load, "The device’s maintenance windows could not be loaded.");
  const current = agent.maintenance?.current ?? null;
  const upcoming = (data?.open ?? []).filter((window) => window.state === "upcoming");

  const act = async (key: string, action: () => Promise<unknown>, fallback: string) => {
    setBusy(key);
    setError(null);
    try {
      await action();
      reload();
    } catch (failure) {
      if (!(failure instanceof AuthenticationRequired)) setError(errorText(failure, fallback));
    } finally {
      setBusy(null);
    }
  };

  if (!current && !upcoming.length && !canChange) return null;
  return (
    <div className="device-maintenance">
      {current && (
        <section className="maintenance-banner" aria-label="Maintenance">
          <Wrench size={18} aria-hidden="true" />
          <div>
            <strong>In maintenance {untilText(current.ends_at, now, MAINTENANCE_FORMAT)}</strong>
            <p>{current.reason}</p>
            <small>Set by {current.created_by} {current.starts_at <= now ? `at ${formatDateTime(current.starts_at)}` : ""}{current.windows > 1 ? ` · ${current.windows} windows in force` : ""}. {holdsBack(current.automation)}</small>
          </div>
          {canChange && (
            <button type="button" className="secondary-button" disabled={busy !== null} onClick={() => void act("end", () => endMaintenance(authorizedFetch, { target: "devices", device_ids: [agent.id] }), "Maintenance could not be ended.")}>
              {busy === "end" && <LoaderCircle size={15} className="spin" aria-hidden="true" />}End now
            </button>
          )}
        </section>
      )}
      {upcoming.length > 0 && (
        <ul className="maintenance-upcoming" aria-label="Upcoming maintenance">
          {upcoming.map((window) => (
            <li key={window.id}>
              <CalendarClock size={15} aria-hidden="true" />
              <span>
                <strong>Maintenance from {formatDateTime(window.starts_at)} {untilText(window.ends_at, window.starts_at, MAINTENANCE_FORMAT)}</strong>
                <small>{window.reason} · {windowSetBy(window)}</small>
              </span>
              {canChange && <button type="button" className="link-button" disabled={busy !== null} onClick={() => void act(window.id, () => endWindow(authorizedFetch, window.id), "The maintenance window could not be cancelled.")}>Cancel</button>}
            </li>
          ))}
        </ul>
      )}
      {error && <p role="alert" className="installer-error">{error}</p>}
      {canChange && !current && (
        <button ref={opener} type="button" className="secondary-button" onClick={() => setStarting(true)} aria-haspopup="dialog">
          <Wrench size={15} aria-hidden="true" />Put in maintenance
        </button>
      )}
      {starting && <StartMaintenanceDialog agents={[agent]} returnFocus={opener} onClose={() => setStarting(false)} onDone={reload} />}
    </div>
  );
}
