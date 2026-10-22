import { ChevronDown, ChevronRight, CircleAlert, LoaderCircle, PackageSearch, RefreshCw, Search } from "lucide-react";
import { useCallback, useEffect, useState } from "react";
import { Link, useSearchParams } from "react-router";
import { AuthenticationRequired, errorText } from "../../lib/http";
import { useResource } from "../../lib/use-resource";
import { NO_COMPANY } from "../agents/device-filters";
import { HeaderActions } from "../workspace/header-actions";
import { useWorkspace } from "../workspace/workspace-context";
import {
  type FleetApplication,
  type InstalledOn,
  MAX_SOFTWARE_QUERY_LENGTH,
  type SoftwareFilters,
  formatDate,
  installedOnParams,
  parseSoftwareFilters,
  softwareParams,
  sourceLabel,
} from "./model";
import { fetchInstalledOn, fetchSoftware } from "./system-info-api";

// Typing reaches the address and the server after a pause.
const QUERY_DELAY_MS = 300;

// `/software`: which devices have an application installed, and in which
// version, across every device or one company's.
export function SoftwarePage() {
  const { authorizedFetch, inventory } = useWorkspace();
  const [searchParams, setSearchParams] = useSearchParams();
  const filters = parseSoftwareFilters(searchParams);
  const [queryInput, setQueryInput] = useState(filters.query);
  const [querySource, setQuerySource] = useState(filters.query);
  // The application whose devices are shown.
  const [open, setOpen] = useState<string | null>(null);
  const choose = (change: Partial<SoftwareFilters>) =>
    setSearchParams(softwareParams({ ...filters, ...change }), { replace: true });

  // Back and forward change the address under the search box.
  if (filters.query !== querySource) {
    setQuerySource(filters.query);
    if (filters.query !== queryInput) setQueryInput(filters.query);
  }

  useEffect(() => {
    if (queryInput === filters.query) return;
    const timer = window.setTimeout(() => choose({ query: queryInput }), QUERY_DELAY_MS);
    return () => window.clearTimeout(timer);
    // choose changes with the address, which this effect already follows.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [queryInput, filters.query, filters.company]);

  const params = softwareParams(filters).toString();
  const load = useCallback(() => fetchSoftware(authorizedFetch, new URLSearchParams(params)), [authorizedFetch, params]);
  const { data, error, reload } = useResource(load, "The applications could not be loaded.");
  const searching = Boolean(filters.query.trim());

  return (
    <>
      <HeaderActions>
        <button type="button" className="icon-button" onClick={reload} aria-label="Refresh" title="Refresh"><RefreshCw size={16} /></button>
      </HeaderActions>
      <section className="agent-panel">
        <div className="table-toolbar">
          <label className="search-field software-search">
            <Search size={16} aria-hidden="true" />
            <input type="search" aria-label="Search applications" value={queryInput} maxLength={MAX_SOFTWARE_QUERY_LENGTH} onChange={(event) => setQueryInput(event.target.value)} placeholder="Search applications" />
          </label>
          <select aria-label="Company" value={filters.company} onChange={(event) => choose({ company: event.target.value })}>
            <option value="">Every company</option>
            <option value={NO_COMPANY}>No company</option>
            {inventory.companies.map((company) => <option key={company.id} value={company.id}>{company.name}</option>)}
          </select>
        </div>
        {!data ? (
          error
            ? <div className="empty-state"><CircleAlert size={22} aria-hidden="true" /><strong>{error}</strong><button type="button" className="secondary-button" onClick={reload}><RefreshCw size={16} /> Try again</button></div>
            : <div className="empty-state"><LoaderCircle size={22} className="spin" aria-hidden="true" /><strong>Loading applications…</strong></div>
        ) : data.applications.length === 0 ? (
          <div className="empty-state">
            <PackageSearch size={22} aria-hidden="true" />
            <strong>{searching ? "No matching applications" : "No applications yet"}</strong>
            <span>{searching
              ? "No device has an application with that in its name."
              : "Devices send their installed applications when they connect and every few hours."}</span>
          </div>
        ) : (
          <>
            {error && <p className="table-message form-error" role="alert"><CircleAlert size={16} aria-hidden="true" />{error}</p>}
            {!searching && <p className="table-message field-help">The applications on the most devices. Search to find any other.</p>}
            <div className="table-scroll">
              <table className="data-table fleet-software">
                <thead><tr><th scope="col">Application</th><th scope="col">Versions</th><th scope="col">Devices</th></tr></thead>
                <tbody>
                  {data.applications.map((application) => (
                    <FleetRow
                      key={application.name}
                      application={application}
                      company={filters.company}
                      open={open === application.name}
                      onToggle={() => setOpen(open === application.name ? null : application.name)}
                    />
                  ))}
                </tbody>
              </table>
            </div>
            {data.truncated && <p className="panel-footer field-help">Showing the first {data.applications.length}. Search to narrow the list.</p>}
          </>
        )}
      </section>
    </>
  );
}

// At most this many versions show beside an application; the rest are counted.
const SHOWN_VERSIONS = 4;

function FleetRow({ application, company, open, onToggle }: {
  application: FleetApplication;
  company: string;
  open: boolean;
  onToggle: () => void;
}) {
  const rest = application.versions.length - SHOWN_VERSIONS;
  const detailsId = `software-${encodeURIComponent(application.name)}`;
  return (
    <>
      <tr>
        <td>
          <button type="button" className="disclosure-button" onClick={onToggle} aria-expanded={open} aria-controls={open ? detailsId : undefined}>
            {open ? <ChevronDown size={15} aria-hidden="true" /> : <ChevronRight size={15} aria-hidden="true" />}
            <strong>{application.name}</strong>
          </button>
          {application.publisher && <span className="row-detail">{application.publisher}</span>}
        </td>
        <td>
          <ul className="version-chips">
            {application.versions.slice(0, SHOWN_VERSIONS).map((version) => (
              <li key={version.version ?? ""}><span className="mono">{version.version ?? "No version"}</span><em>{version.devices}</em></li>
            ))}
            {rest > 0 && <li className="more">+{rest} more</li>}
          </ul>
        </td>
        <td className="numeric">{application.devices}</td>
      </tr>
      {open && (
        <tr className="detail-row" id={detailsId}>
          <td colSpan={3}><InstalledDevices name={application.name} company={company} /></td>
        </tr>
      )}
    </>
  );
}

function InstalledDevices({ name, company }: { name: string; company: string }) {
  const { authorizedFetch } = useWorkspace();
  const [state, setState] = useState<{ devices: InstalledOn[] | null; error: string | null }>({ devices: null, error: null });

  useEffect(() => {
    let cancelled = false;
    fetchInstalledOn(authorizedFetch, installedOnParams(name, undefined, company)).then(
      (devices) => !cancelled && setState({ devices, error: null }),
      (error: unknown) => {
        if (!cancelled && !(error instanceof AuthenticationRequired)) setState({ devices: null, error: errorText(error, "The devices could not be loaded.") });
      },
    );
    return () => {
      cancelled = true;
    };
  }, [authorizedFetch, name, company]);

  if (state.error) return <p className="form-error" role="alert"><CircleAlert size={16} aria-hidden="true" />{state.error}</p>;
  if (!state.devices) return <p className="page-status" role="status"><LoaderCircle size={16} className="spin" aria-hidden="true" /> Loading devices…</p>;
  return (
    <table className="data-table nested-table">
      <thead><tr><th scope="col">Device</th><th scope="col">Version</th><th scope="col">Installed</th><th scope="col">Source</th></tr></thead>
      <tbody>
        {state.devices.map((device, index) => (
          <tr key={`${device.device_id}-${device.version}-${index}`}>
            <td><Link to={`/device?${new URLSearchParams({ id: device.device_id, tab: "software", q: name })}`}>{device.device_name}</Link></td>
            <td className="mono">{device.version ?? "—"}{device.architecture && <span className="badge badge-muted">{device.architecture}</span>}</td>
            <td className="numeric">{formatDate(device.installed_at)}</td>
            <td>{sourceLabel(device.source)}</td>
          </tr>
        ))}
      </tbody>
    </table>
  );
}
