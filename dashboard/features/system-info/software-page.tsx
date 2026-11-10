import { ChevronDown, ChevronRight, CircleAlert, LoaderCircle, PackageSearch, RefreshCw, Search } from "lucide-react";
import { useCallback, useEffect, useState } from "react";
import { Link, useSearchParams } from "react-router";
import { ShowMore } from "../../lib/show-more";
import { usePagedList } from "../../lib/use-paged-list";
import { SEARCH_DELAY_MS } from "../../lib/use-settled";
import { NO_COMPANY } from "../agents/device-filters";
import { HeaderActions } from "../workspace/header-actions";
import { useWorkspace } from "../workspace/workspace-context";
import { useCompanies, useTags } from "../agents/use-devices";
import {
  type FleetApplication,
  MAX_SOFTWARE_QUERY_LENGTH,
  SOFTWARE_PAGE_SIZE,
  type SoftwareFilters,
  applicationName,
  copyId,
  formatDate,
  installedOnParams,
  parseSoftwareFilters,
  softwareKey,
  softwareParams,
  sourceLabel,
} from "./model";
import { fetchInstalledOn, fetchSoftware } from "./system-info-api";

const APPLICATION_MESSAGES = { first: "The applications could not be loaded.", more: "More applications could not be loaded." };
const COPY_MESSAGES = { first: "The devices could not be loaded.", more: "More devices could not be loaded." };

const count = (number: number, one: string, many: string) => `${number} ${number === 1 ? one : many}`;

// `/software`: which devices have an application installed, and in which
// version, across every device or one company's or tag's. The server
// searches, orders and counts the applications and sends them a page at a
// time, and an opened application's devices the same way.
export function SoftwarePage() {
  const { authorizedFetch, inventory } = useWorkspace();
  const companies = useCompanies(inventory.devices);
  const tags = useTags(inventory.devices);
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

  // Typing reaches the address, and so the server, after a pause.
  useEffect(() => {
    if (queryInput === filters.query) return;
    const timer = window.setTimeout(() => choose({ query: queryInput }), SEARCH_DELAY_MS);
    return () => window.clearTimeout(timer);
    // choose changes with the address, which this effect already follows.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [queryInput, filters.query, filters.company, filters.tag]);

  const key = softwareKey(filters);
  const fetchPage = useCallback(async (offset: number) => {
    const page = await fetchSoftware(authorizedFetch, parseSoftwareFilters(new URLSearchParams(key)), offset);
    return { items: page.applications, matching: page.matching };
  }, [authorizedFetch, key]);
  const { list, error, reload, showMore, loadingMore, remaining, current } = usePagedList(key, fetchPage, applicationName, APPLICATION_MESSAGES);
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
            {companies.map((company) => <option key={company.id} value={company.id}>{company.name}</option>)}
          </select>
          {(tags.length > 0 || filters.tag) && (
            <select aria-label="Tag" value={filters.tag} onChange={(event) => choose({ tag: event.target.value })}>
              <option value="">Every tag</option>
              {filters.tag && !tags.some((tag) => tag.id === filters.tag) && <option value={filters.tag}>Unknown tag</option>}
              {tags.map((tag) => <option key={tag.id} value={tag.id}>{tag.name}</option>)}
            </select>
          )}
        </div>
        {!list ? (
          error
            ? <div className="empty-state"><CircleAlert size={22} aria-hidden="true" /><strong>{error}</strong><button type="button" className="secondary-button" onClick={reload}><RefreshCw size={16} /> Try again</button></div>
            : <div className="empty-state"><LoaderCircle size={22} className="spin" aria-hidden="true" /><strong>Loading applications…</strong></div>
        ) : list.items.length === 0 && remaining === 0 ? (
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
            {!searching && <p className="table-message field-help">The applications on the most devices first.</p>}
            <div className="table-scroll">
              <table className="data-table fleet-software" aria-busy={!current}>
                <thead><tr><th scope="col">Application</th><th scope="col">Versions</th><th scope="col">Devices</th></tr></thead>
                <tbody>
                  {list.items.map((application) => (
                    <FleetRow
                      key={application.name}
                      application={application}
                      company={filters.company}
                      tag={filters.tag}
                      open={open === application.name}
                      onToggle={() => setOpen(open === application.name ? null : application.name)}
                    />
                  ))}
                </tbody>
              </table>
            </div>
            <div className="panel-footer fleet-footer">
              <span className="field-help">
                {count(list.matching, "application", "applications")}
                {remaining > 0 && ` · ${list.items.length} loaded`}
              </span>
              <ShowMore remaining={remaining} pageSize={SOFTWARE_PAGE_SIZE} loading={loadingMore} disabled={!current} onShowMore={() => void showMore()} />
            </div>
          </>
        )}
      </section>
    </>
  );
}

// At most this many versions show beside an application; the rest are counted.
const SHOWN_VERSIONS = 4;

function FleetRow({ application, company, tag, open, onToggle }: {
  application: FleetApplication;
  company: string;
  tag: string;
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
          <td colSpan={3}><InstalledDevices name={application.name} company={company} tag={tag} /></td>
        </tr>
      )}
    </>
  );
}

// The devices that have one application, a page at a time.
function InstalledDevices({ name, company, tag }: { name: string; company: string; tag: string }) {
  const { authorizedFetch } = useWorkspace();
  const key = installedOnParams(name, undefined, company, tag).toString();
  const fetchPage = useCallback(async (offset: number) => {
    const page = await fetchInstalledOn(authorizedFetch, new URLSearchParams(key), offset);
    return { items: page.devices, matching: page.matching };
  }, [authorizedFetch, key]);
  const { list, error, showMore, loadingMore, remaining, current } = usePagedList(key, fetchPage, copyId, COPY_MESSAGES);

  if (!list) {
    return error
      ? <p className="form-error" role="alert"><CircleAlert size={16} aria-hidden="true" />{error}</p>
      : <p className="page-status" role="status"><LoaderCircle size={16} className="spin" aria-hidden="true" /> Loading devices…</p>;
  }
  return (
    <>
      {error && <p className="form-error" role="alert"><CircleAlert size={16} aria-hidden="true" />{error}</p>}
      <table className="data-table nested-table" aria-busy={!current}>
        <thead><tr><th scope="col">Device</th><th scope="col">Version</th><th scope="col">Installed</th><th scope="col">Source</th></tr></thead>
        <tbody>
          {list.items.map((device) => (
            <tr key={copyId(device)}>
              <td><Link to={`/device?${new URLSearchParams({ id: device.device_id, tab: "software", q: name })}`}>{device.device_name}</Link></td>
              <td className="mono">{device.version ?? "—"}{device.architecture && <span className="badge badge-muted">{device.architecture}</span>}</td>
              <td className="numeric">{formatDate(device.installed_at)}</td>
              <td>{sourceLabel(device.source)}</td>
            </tr>
          ))}
        </tbody>
      </table>
      {(remaining > 0 || list.matching > SOFTWARE_PAGE_SIZE) && (
        <div className="panel-footer fleet-footer">
          <span className="field-help">
            {count(list.matching, "copy", "copies")}
            {remaining > 0 && ` · ${list.items.length} loaded`}
          </span>
          <ShowMore remaining={remaining} pageSize={SOFTWARE_PAGE_SIZE} loading={loadingMore} disabled={!current} onShowMore={() => void showMore()} />
        </div>
      )}
    </>
  );
}
