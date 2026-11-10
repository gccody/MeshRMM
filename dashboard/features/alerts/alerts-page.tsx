import { BellOff, CircleAlert, LoaderCircle, RefreshCw, X } from "lucide-react";
import { useState } from "react";
import { useSearchParams } from "react-router";
import { DeviceSelect } from "../device-picker/device-select";
import { useWorkspace } from "../workspace/workspace-context";
import { useCompanies, useDevices } from "../agents/use-devices";
import { HeaderActions } from "../workspace/header-actions";
import { AlertTable } from "./alert-table";
import { AlertAreaTabs } from "./area-tabs";
import {
  type AlertFilters,
  NO_COMPANY_FILTER,
  SEVERITIES,
  SEVERITY_LABELS,
  type StateFilter,
  alertFilterParams,
  parseAlertFilters,
} from "./model";
import { useLiveAlerts } from "./use-live-alerts";

const STATES: { id: StateFilter; label: string }[] = [
  { id: "active", label: "Active" },
  { id: "open", label: "Open" },
  { id: "acknowledged", label: "Acknowledged" },
  { id: "resolved", label: "Resolved" },
  { id: "all", label: "All" },
];

// `/alerts`: alerts the rules raised, open ones first, filtered by state,
// severity, company and device. The event socket keeps the list current;
// it holds the first ALERT_PAGE_LIMIT, and says when more match.
export function AlertsPage() {
  const { inventory } = useWorkspace();
  const [searchParams, setSearchParams] = useSearchParams();
  const filters = parseAlertFilters(searchParams);
  const query = alertFilterParams(filters).toString();
  const [actionError, setActionError] = useState<string | null>(null);

  const { data, error, reload, apply } = useLiveAlerts(query);

  const choose = (change: Partial<AlertFilters>) => {
    setSearchParams(alertFilterParams({ ...filters, ...change }), { replace: true });
  };

  const companies = useCompanies(inventory.devices);
  const agents = useDevices(inventory.devices);
  const filtered = filters.state !== "active" || filters.severity || filters.company || filters.device;

  return (
    <>
      <HeaderActions>
        <button type="button" className="icon-button" onClick={reload} aria-label="Refresh" title="Refresh"><RefreshCw size={16} /></button>
      </HeaderActions>
      <AlertAreaTabs current="alerts" />
      {actionError && (
        <div className="error-banner" role="alert">
          <CircleAlert size={17} aria-hidden="true" /><span>{actionError}</span>
          <button onClick={() => setActionError(null)} aria-label="Dismiss"><X size={16} /></button>
        </div>
      )}
      <section className="agent-panel">
        <div className="table-toolbar alert-filters">
          <div className="segmented" role="group" aria-label="State">
            {STATES.map((state) => (
              <button key={state.id} type="button" aria-pressed={filters.state === state.id} onClick={() => choose({ state: state.id })}>{state.label}</button>
            ))}
          </div>
          <div className="alert-filter-selects">
            <select aria-label="Severity" value={filters.severity} onChange={(event) => choose({ severity: event.target.value as AlertFilters["severity"] })}>
              <option value="">Every severity</option>
              {SEVERITIES.map((severity) => <option key={severity} value={severity}>{SEVERITY_LABELS[severity]}</option>)}
            </select>
            <select aria-label="Company" value={filters.company} onChange={(event) => choose({ company: event.target.value })}>
              <option value="">Every company</option>
              <option value={NO_COMPANY_FILTER}>No company</option>
              {companies.map((company) => <option key={company.id} value={company.id}>{company.name}</option>)}
            </select>
            <DeviceSelect label="Device" agents={agents} value={filters.device} onChange={(device) => choose({ device })} none="Every device" />
          </div>
        </div>
        {!data || (data.truncated && data.alerts.length === 0) ? (
          error
            ? <div className="empty-state"><CircleAlert size={22} aria-hidden="true" /><strong>{error}</strong><button type="button" className="secondary-button" onClick={reload}><RefreshCw size={16} /> Try again</button></div>
            : <div className="empty-state"><LoaderCircle size={22} className="spin" aria-hidden="true" /><strong>Loading alerts…</strong></div>
        ) : data.alerts.length === 0 ? (
          <div className="empty-state">
            <BellOff size={22} aria-hidden="true" />
            <strong>{filtered ? "No matching alerts" : "No active alerts"}</strong>
            <span>{filtered
              ? "Nothing matches these filters."
              : "When a rule's condition holds for a device, its alert appears here. Rules are on the Rules tab."}</span>
            {filtered && <button type="button" className="secondary-button" onClick={() => setSearchParams(new URLSearchParams(), { replace: true })}>Clear filters</button>}
          </div>
        ) : (
          <>
            {error && <p className="table-message form-error" role="alert"><CircleAlert size={16} aria-hidden="true" />{error}</p>}
            <AlertTable
              alerts={data.alerts}
              showDevice
              label="Alerts"
              onError={setActionError}
              onChanged={apply}
            />
            {data.truncated && <p className="panel-footer field-help">Showing the newest {data.alerts.length}. Narrow the filters to see others.</p>}
          </>
        )}
      </section>
    </>
  );
}
