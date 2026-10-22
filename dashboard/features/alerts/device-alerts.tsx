import { CircleAlert, LoaderCircle } from "lucide-react";
import { useCallback, useState } from "react";
import { Link } from "react-router";
import { useResource } from "../../lib/use-resource";
import { useWorkspace } from "../workspace/workspace-context";
import { fetchAlerts } from "./alerts-api";
import { AlertTable } from "./alert-table";
import { DEFAULT_ALERT_FILTERS, alertFilterParams, applyAlertChange } from "./model";
import { useAlertEvents } from "./use-alert-feed";

const SHOWN = 20;

// A device's alerts on its page: unresolved ones first, then the latest
// resolved ones.
export function DeviceAlerts({ deviceId }: { deviceId: string }) {
  const { authorizedFetch } = useWorkspace();
  const [actionError, setActionError] = useState<string | null>(null);
  const filters = { ...DEFAULT_ALERT_FILTERS, state: "all" as const, device: deviceId };
  const query = `${alertFilterParams(filters).toString()}&limit=${SHOWN}`;
  const load = useCallback(() => fetchAlerts(authorizedFetch, query), [authorizedFetch, query]);
  const { data, error, reload, setData } = useResource(load, "The device's alerts could not be loaded.");

  useAlertEvents((event) => {
    if (event.type === "alert_counts") reload();
    else if (event.alert.device_id === deviceId) {
      setData((current) => ({ ...current, alerts: applyAlertChange(current.alerts, event.alert, filters).slice(0, SHOWN) }));
    }
  });

  const unresolved = data?.alerts.filter((alert) => alert.state !== "resolved").length ?? 0;
  return (
    <section className="device-alerts" aria-labelledby="device-alerts-title">
      <div className="device-alerts-heading">
        <h3 id="device-alerts-title">Alerts{unresolved > 0 && <span className="badge badge-warn">{unresolved} unresolved</span>}</h3>
        <Link to={`/alerts?${alertFilterParams(filters).toString()}`} className="back-link">See all</Link>
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
            onChanged={(alert) => setData((current) => ({ ...current, alerts: applyAlertChange(current.alerts, alert, filters) }))}
          />
        )}
      </div>
    </section>
  );
}
