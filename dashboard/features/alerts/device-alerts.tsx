import { CircleAlert, LoaderCircle } from "lucide-react";
import { useState } from "react";
import { Link } from "react-router";
import { AlertTable } from "./alert-table";
import { DEFAULT_ALERT_FILTERS, alertFilterParams } from "./model";
import { useLiveAlerts } from "./use-live-alerts";

const SHOWN = 20;

// A device's alerts on its page: unresolved ones first, then the latest
// resolved ones. Kept current as the Alerts page is: other devices' alerts
// are ignored, and the socket's counts read the list again at most every
// few seconds and not while the tab is hidden.
export function DeviceAlerts({ deviceId }: { deviceId: string }) {
  const [actionError, setActionError] = useState<string | null>(null);
  const query = alertFilterParams({ ...DEFAULT_ALERT_FILTERS, state: "all", device: deviceId }).toString();
  const { data, error, apply } = useLiveAlerts(query, { limit: SHOWN, failed: "The device's alerts could not be loaded." });

  const unresolved = data?.alerts.filter((alert) => alert.state !== "resolved").length ?? 0;
  return (
    <section className="device-alerts" aria-labelledby="device-alerts-title">
      <div className="device-alerts-heading">
        <h3 id="device-alerts-title">Alerts{unresolved > 0 && <span className="badge badge-warn">{unresolved} unresolved</span>}</h3>
        <Link to={`/alerts?${query}`} className="back-link">See all</Link>
      </div>
      {actionError && <p className="form-error" role="alert"><CircleAlert size={16} aria-hidden="true" />{actionError}</p>}
      <div className="agent-panel">
        {!data ? (
          error
            ? <p className="table-message form-error" role="alert"><CircleAlert size={16} aria-hidden="true" />{error}</p>
            : <p className="table-message" role="status"><LoaderCircle size={16} className="spin" /> Loading alerts…</p>
        ) : data.alerts.length === 0 ? (
          <p className="table-message field-help">No alerts for this device.</p>
        ) : (
          <AlertTable
            alerts={data.alerts}
            showDevice={false}
            label="This device's alerts"
            onError={setActionError}
            onChanged={apply}
          />
        )}
      </div>
    </section>
  );
}
