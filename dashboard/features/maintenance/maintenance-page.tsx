import { CircleAlert, LoaderCircle, Plus, Wrench, X } from "lucide-react";
import { useCallback, useMemo, useRef, useState } from "react";
import { Link, useSearchParams } from "react-router";
import { AuthenticationRequired, errorText } from "../../lib/http";
import { formatDateTime } from "../../lib/format";
import { useResource } from "../../lib/use-resource";
import { useDevices } from "../agents/use-devices";
import { GroupingTabs } from "../companies/area-tabs";
import { HeaderActions } from "../workspace/header-actions";
import { useWorkspace } from "../workspace/workspace-context";
import { MAINTENANCE_FORMAT } from "./device-maintenance";
import { endWindow, fetchWindows } from "./maintenance-api";
import { StartMaintenanceDialog } from "./maintenance-dialog";
import { AUTOMATION_CHOICES, type MaintenanceWindow, SHOWING_LABELS, type Showing, WINDOW_STATE_LABELS, untilText, windowSetBy } from "./model";

const SHOWINGS = Object.keys(SHOWING_LABELS) as Showing[];
const isShowing = (value: string | null): value is Showing => SHOWINGS.includes(value as Showing);
const devicePath = (id: string) => `/device?id=${encodeURIComponent(id)}`;

// `/maintenance?state=…`: the maintenance windows in force and to come, or
// those past, with a way to start more and to end them. It reads them again
// whenever a device's maintenance changes.
export function MaintenancePage() {
  const { authorizedFetch, can, inventory } = useWorkspace();
  const canChange = can("devices.maintenance");
  const [searchParams, setSearchParams] = useSearchParams();
  const requested = searchParams.get("state");
  const showing: Showing = isShowing(requested) ? requested : "open";
  const [starting, setStarting] = useState(false);
  const [busyId, setBusyId] = useState<string | null>(null);
  const [actionError, setActionError] = useState<string | null>(null);
  const opener = useRef<HTMLButtonElement>(null);
  // Every device's maintenance as presence shows it: when it changes, the
  // list is read again.
  const agents = useDevices(inventory.devices);
  const changeKey = useMemo(() => agents.filter((agent) => agent.maintenance).map((agent) => `${agent.id}:${JSON.stringify(agent.maintenance)}`).join("|"), [agents]);
  const load = useCallback(() => {
    void changeKey;
    return fetchWindows(authorizedFetch, showing);
  }, [authorizedFetch, showing, changeKey]);
  const { data, error, reload } = useResource(load, "The maintenance windows could not be loaded.");

  const end = async (window: MaintenanceWindow) => {
    const verb = window.state === "upcoming" ? "Cancel" : "End";
    if (!globalThis.confirm(`${verb} the maintenance window on ${window.device_name}?`)) return;
    setBusyId(window.id);
    setActionError(null);
    try {
      await endWindow(authorizedFetch, window.id);
      reload();
    } catch (failure) {
      if (!(failure instanceof AuthenticationRequired)) setActionError(errorText(failure, "The maintenance window could not be ended."));
    } finally {
      setBusyId(null);
    }
  };

  return (
    <>
      {(actionError || error) && (
        <div className="error-banner" role="alert">
          <CircleAlert size={17} aria-hidden="true" /><span>{actionError ?? error}</span>
          {actionError && <button onClick={() => setActionError(null)} aria-label="Dismiss"><X size={16} /></button>}
        </div>
      )}
      <HeaderActions>
        <div className="segmented" role="group" aria-label="Show windows">
          {SHOWINGS.map((choice) => (
            <button key={choice} type="button" aria-pressed={showing === choice} onClick={() => setSearchParams(choice === "open" ? {} : { state: choice }, { replace: true })}>{SHOWING_LABELS[choice]}</button>
          ))}
        </div>
        {canChange && <button ref={opener} type="button" className="primary-button" onClick={() => setStarting(true)} aria-haspopup="dialog"><Plus size={16} /> Start maintenance</button>}
      </HeaderActions>
      <GroupingTabs current="maintenance" />
      <section className="agent-panel">
        {!data ? (
          <div className="empty-state"><LoaderCircle size={22} className="spin" aria-hidden="true" /><strong>Loading maintenance windows…</strong></div>
        ) : data.windows.length === 0 ? (
          <div className="empty-state">
            <Wrench size={22} aria-hidden="true" />
            <strong>{showing === "past" ? "No past maintenance" : "No devices are in maintenance"}</strong>
            {showing !== "past" && <span>A device in maintenance raises no alerts and its scheduled automation is held back. Put devices in maintenance from the device wall, a device&apos;s page or here, or on a schedule with a maintenance policy.</span>}
          </div>
        ) : (
          <div className="table-scroll">
            <table className="data-table maintenance-table" aria-label="Maintenance windows">
              <thead><tr><th scope="col">Device</th><th scope="col">When</th><th scope="col">Reason</th><th scope="col">Automation</th><th scope="col">State</th><th scope="col"><span className="sr-only">Actions</span></th></tr></thead>
              <tbody>
                {data.windows.map((window) => (
                  <tr key={window.id}>
                    <td><strong><Link to={devicePath(window.device_id)}>{window.device_name}</Link></strong>{window.company_name && <span>{window.company_name}</span>}</td>
                    <td>{formatDateTime(window.starts_at)} {untilText(window.ends_at, window.starts_at, MAINTENANCE_FORMAT)}</td>
                    <td>{window.reason}<span>{windowSetBy(window)}</span></td>
                    <td>{AUTOMATION_CHOICES[window.automation].label}</td>
                    <td><span className={`badge${window.state === "current" ? " badge-warn" : " badge-muted"}`}>{WINDOW_STATE_LABELS[window.state]}</span></td>
                    <td className="row-actions">
                      {canChange && (window.state === "current" || window.state === "upcoming") && (
                        <button type="button" className="secondary-button" disabled={busyId === window.id} onClick={() => void end(window)}>
                          {busyId === window.id && <LoaderCircle size={15} className="spin" aria-hidden="true" />}{window.state === "upcoming" ? "Cancel" : "End now"}
                        </button>
                      )}
                    </td>
                  </tr>
                ))}
              </tbody>
            </table>
            {data.truncated && <p className="field-help">Showing the first {data.windows.length}.</p>}
          </div>
        )}
      </section>
      {starting && <StartMaintenanceDialog agents={null} returnFocus={opener} onClose={() => setStarting(false)} onDone={reload} />}
    </>
  );
}
