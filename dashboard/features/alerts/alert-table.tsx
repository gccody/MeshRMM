import { Check, CheckCheck, LoaderCircle } from "lucide-react";
import { useState } from "react";
import { Link } from "react-router";
import { AuthenticationRequired, errorText } from "../../lib/http";
import { formatDateTime, formatRelative } from "../../lib/format";
import { useWorkspace } from "../workspace/workspace-context";
import { respondToAlert } from "./alerts-api";
import { type Alert, SEVERITY_LABELS, type Severity, clearsItself, resolutionLabel } from "./model";

export function SeverityBadge({ severity }: { severity: Severity }) {
  return <span className={`severity severity-${severity}`}><i aria-hidden="true" />{SEVERITY_LABELS[severity]}</span>;
}

// Alerts with their state, and Acknowledge and Resolve for users who may.
export function AlertTable({ alerts, showDevice, onChanged, onError, label }: {
  alerts: Alert[];
  showDevice: boolean;
  onChanged: (alert: Alert) => void;
  onError: (message: string | null) => void;
  label: string;
}) {
  const { authorizedFetch, can, inventory, alerts: feed } = useWorkspace();
  const [busy, setBusy] = useState<string | null>(null);
  const canRespond = can("alerts.respond");

  const respond = async (alert: Alert, action: "acknowledge" | "resolve") => {
    if (action === "resolve" && clearsItself(alert.condition) && !window.confirm(
      `Resolve “${alert.rule_name}” on ${alert.device_name}? It resolves itself when its condition passes; if the condition still holds, the rule raises it again.`,
    )) return;
    setBusy(`${alert.id}:${action}`);
    onError(null);
    try {
      onChanged(await respondToAlert(authorizedFetch, alert, action));
      // The event socket brings the new counts; without it, read them.
      if (inventory.connection !== "live") void feed.refreshCounts();
    } catch (error) {
      if (!(error instanceof AuthenticationRequired)) onError(errorText(error, "The alert could not be changed."));
    } finally {
      setBusy(null);
    }
  };

  return (
    <table className="data-table alert-table" aria-label={label}>
      <thead>
        <tr>
          <th scope="col">Severity</th>
          <th scope="col">Alert</th>
          {showDevice && <th scope="col">Device</th>}
          <th scope="col">Raised</th>
          <th scope="col">State</th>
          <th scope="col"><span className="sr-only">Actions</span></th>
        </tr>
      </thead>
      <tbody>
        {alerts.map((alert) => (
          <tr key={alert.id} className={alert.state === "resolved" ? "row-disabled" : undefined}>
            <td><SeverityBadge severity={alert.severity} /></td>
            <td>
              <strong>{alert.rule_name}</strong>
              <span>{alert.detail}{alert.occurrences > 1 ? ` · ${alert.occurrences} times, last ${formatRelative(alert.last_occurred_at)}` : ""}</span>
              {(alert.update_run_id || alert.condition === "updates_pending" || alert.condition === "reboot_pending") && can("updates.view") && <span><Link to={`/device?${new URLSearchParams({ id: alert.device_id, tab: "updates" })}`}>See the device’s updates</Link></span>}
            </td>
            {showDevice && (
              <td>
                <strong>{can("devices.view") ? <Link to={`/device?id=${encodeURIComponent(alert.device_id)}`}>{alert.device_name}</Link> : alert.device_name}</strong>
                <span>{alert.company_name ?? "No company"}</span>
              </td>
            )}
            <td><time dateTime={new Date(alert.raised_at).toISOString()} title={formatDateTime(alert.raised_at)}>{formatRelative(alert.raised_at)}</time></td>
            <td><AlertStateCell alert={alert} /></td>
            <td className="row-actions">
              {canRespond && alert.state === "open" && (
                <button type="button" className="secondary-button" disabled={busy !== null} onClick={() => void respond(alert, "acknowledge")} title="Let others know someone is on it">
                  {busy === `${alert.id}:acknowledge` ? <LoaderCircle size={15} className="spin" /> : <Check size={15} />} Acknowledge
                </button>
              )}
              {canRespond && alert.state !== "resolved" && (
                <button type="button" className="secondary-button" disabled={busy !== null} onClick={() => void respond(alert, "resolve")}>
                  {busy === `${alert.id}:resolve` ? <LoaderCircle size={15} className="spin" /> : <CheckCheck size={15} />} Resolve
                </button>
              )}
            </td>
          </tr>
        ))}
      </tbody>
    </table>
  );
}

function AlertStateCell({ alert }: { alert: Alert }) {
  if (alert.state === "open") return <span className="run-pill run-pill-problem">Open</span>;
  if (alert.state === "acknowledged") {
    return (
      <>
        <span className="run-pill run-pill-pending">Acknowledged</span>
        <span title={formatDateTime(alert.acknowledged_at)}>by {alert.acknowledged_by ?? "someone"}</span>
      </>
    );
  }
  return (
    <>
      <span className="run-pill run-pill-success">Resolved</span>
      <span title={formatDateTime(alert.resolved_at)}>{resolutionLabel(alert)}{alert.resolved_at ? `, ${formatRelative(alert.resolved_at)}` : ""}</span>
    </>
  );
}
