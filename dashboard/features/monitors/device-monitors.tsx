import { CircleAlert, LoaderCircle, Radar, RefreshCw } from "lucide-react";
import { useCallback, useState } from "react";
import { Link } from "react-router";
import { formatDateTime, formatRelative } from "../../lib/format";
import { AuthenticationRequired, errorText } from "../../lib/http";
import { useResource } from "../../lib/use-resource";
import type { Agent } from "../agents/types";
import { SeverityBadge } from "../alerts/alert-table";
import { useAlertEvents } from "../alerts/use-alert-feed";
import { useWorkspace } from "../workspace/workspace-context";
import { REMEDIATION_STATE_LABELS, applyMonitorUpdate, formatSeconds, monitorStatus } from "./model";
import { checkMonitors, fetchDeviceMonitors } from "./monitors-api";

// `/device?id=…&tab=monitors`: the monitor rules that apply to the device,
// what each last found, and Check now.
export function DeviceMonitors({ agent }: { agent: Agent }) {
  const { authorizedFetch, can } = useWorkspace();
  const canCheck = can("alerts.respond");
  const load = useCallback(() => fetchDeviceMonitors(authorizedFetch, agent.id), [authorizedFetch, agent.id]);
  const { data, error, reload, setData } = useResource(load, "The device's monitors could not be loaded.");
  const [busy, setBusy] = useState<string | null>(null);
  const [actionError, setActionError] = useState<string | null>(null);

  useAlertEvents((event) => {
    if (event.type === "monitor_update") {
      if (event.device_id === agent.id) setData((current) => applyMonitorUpdate(current, event));
    } else if (event.type === "alert_counts" || event.alert.device_id === agent.id) {
      // The monitors show their alert's state.
      reload();
    }
  });

  const check = async (ruleIds: string[]) => {
    setBusy(ruleIds[0] ?? "all");
    setActionError(null);
    try {
      await checkMonitors(authorizedFetch, agent.id, ruleIds);
    } catch (failure) {
      if (!(failure instanceof AuthenticationRequired)) setActionError(errorText(failure, "The device could not be asked to check."));
    } finally {
      setBusy(null);
    }
  };

  const runnable = data?.filter((monitor) => !monitor.problem) ?? [];
  return (
    <section className="device-alerts" aria-labelledby="device-monitors-title">
      <div className="device-alerts-heading">
        <h3 id="device-monitors-title">Monitors</h3>
        {canCheck && runnable.length > 0 && (
          <button type="button" className="secondary-button" disabled={busy !== null || !agent.connected} onClick={() => void check([])} title={agent.connected ? "Run every monitor now" : "The device is offline"}>
            {busy === "all" ? <LoaderCircle size={15} className="spin" /> : <RefreshCw size={15} />} Check all now
          </button>
        )}
      </div>
      {actionError && <p className="form-error" role="alert"><CircleAlert size={16} aria-hidden="true" />{actionError}</p>}
      <div className="agent-panel">
        {!data ? (
          error
            ? <p className="table-message form-error" role="alert"><CircleAlert size={16} aria-hidden="true" />{error}</p>
            : <p className="table-message" role="status"><LoaderCircle size={16} className="spin" /> Loading monitors…</p>
        ) : data.length === 0 ? (
          <div className="empty-state compact">
            <Radar size={22} aria-hidden="true" />
            <strong>No monitors watch this device</strong>
            <span>Monitors are alert rules the device&apos;s Agent checks itself, such as a service that must run or a website that must answer.{can("alerts.manage") && <> <Link to="/alert-rules">Add one on the Alert rules page.</Link></>}</span>
          </div>
        ) : (
          <table className="data-table alert-table" aria-label="This device's monitors">
            <thead>
              <tr>
                <th scope="col">Severity</th>
                <th scope="col">Monitor</th>
                <th scope="col">Last result</th>
                <th scope="col">Checked</th>
                <th scope="col"><span className="sr-only">Actions</span></th>
              </tr>
            </thead>
            <tbody>
              {data.map((monitor) => {
                const status = monitorStatus(monitor);
                const { state } = monitor;
                return (
                  <tr key={monitor.rule_id}>
                    <td><SeverityBadge severity={monitor.severity} /></td>
                    <td>
                      <strong>{monitor.rule_name}</strong>
                      <span>{monitor.description}, checked every {formatSeconds(monitor.interval_seconds)}</span>
                      {monitor.remediation && <span>Self-healing: {monitor.remediation}</span>}
                    </td>
                    <td>
                      <span className={`run-pill run-pill-${status.tone === "muted" ? "pending" : status.tone}`}>{status.label}</span>
                      <span>{monitor.problem ?? state?.detail ?? ""}</span>
                      {monitor.alert_id && (
                        <span>
                          <Link to={`/alerts?${new URLSearchParams({ device: agent.id })}`}>Alert {monitor.alert_state ?? "open"}</Link>
                          {monitor.remediation_state && ` · ${REMEDIATION_STATE_LABELS[monitor.remediation_state]}`}
                        </span>
                      )}
                      {state?.output && (
                        <details className="alert-output">
                          <summary>Output</summary>
                          <pre>{state.output}</pre>
                        </details>
                      )}
                    </td>
                    <td>{state ? <time dateTime={new Date(state.checked_at).toISOString()} title={formatDateTime(state.checked_at)}>{formatRelative(state.checked_at)}</time> : "Never"}</td>
                    <td className="row-actions">
                      {canCheck && !monitor.problem && (
                        <button type="button" className="secondary-button" disabled={busy !== null || !agent.connected} onClick={() => void check([monitor.rule_id])} title={agent.connected ? "Run this monitor now" : "The device is offline"}>
                          {busy === monitor.rule_id ? <LoaderCircle size={15} className="spin" /> : <RefreshCw size={15} />} Check now
                        </button>
                      )}
                    </td>
                  </tr>
                );
              })}
            </tbody>
          </table>
        )}
      </div>
    </section>
  );
}
