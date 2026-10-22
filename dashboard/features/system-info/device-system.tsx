import { ArrowDown, ArrowUp, CircleAlert, LoaderCircle, PackageSearch, RefreshCw, Search, ServerOff, TriangleAlert } from "lucide-react";
import { type ReactNode, useEffect, useMemo, useState } from "react";
import { useSearchParams } from "react-router";
import { AuthenticationRequired, errorText } from "../../lib/http";
import { formatDateTime, formatRelative } from "../../lib/format";
import type { Agent } from "../agents/types";
import { formatBytes } from "../metrics/model";
import { useWorkspace } from "../workspace/workspace-context";
import {
  ADAPTER_KIND_LABELS,
  type Application,
  type ApplicationChange,
  type ApplicationSort,
  type ApplicationSortKey,
  DEFAULT_APPLICATION_SORT,
  type Snapshot,
  describeOs,
  formatDate,
  formatLinkSpeed,
  nextSort,
  sortAdapters,
  sourceLabel,
  visibleApplications,
} from "./model";
import { fetchApplications, fetchChanges, fetchSnapshot, requestRefresh } from "./system-info-api";

export type SystemTab = "hardware" | "software" | "network";

// How long a refresh waits for the device before saying it hasn't answered.
const REFRESH_PATIENCE_MS = 60_000;

// Loads `load` again whenever any of `keys` changes, keeping what was
// loaded before on screen meanwhile. `undefined` is still loading.
function useLoaded<T>(load: () => Promise<T>, keys: readonly unknown[], fallback: string) {
  const [state, setState] = useState<{ data: T | undefined; error: string | null }>({ data: undefined, error: null });
  useEffect(() => {
    let cancelled = false;
    load().then(
      (data) => !cancelled && setState({ data, error: null }),
      (error: unknown) => {
        if (cancelled || error instanceof AuthenticationRequired) return;
        setState((current) => ({ data: current.data, error: errorText(error, fallback) }));
      },
    );
    return () => {
      cancelled = true;
    };
    // The keys say when to load again; `load` is a new function each render.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, keys);
  return state;
}

// A device's hardware, software or network, from its latest snapshot, with
// when it was collected and a button that asks the device for a new one. The
// device list's summary changes when a new snapshot arrives, which loads it.
export function DeviceSystem({ agent, tab }: { agent: Agent; tab: SystemTab }) {
  const { authorizedFetch } = useWorkspace();
  const receivedAt = agent.system?.received_at ?? null;
  const { data: snapshot, error } = useLoaded(
    () => fetchSnapshot(authorizedFetch, agent.id),
    [authorizedFetch, agent.id, receivedAt],
    "The device’s system information could not be loaded.",
  );

  return (
    <section className="system-info" aria-label="System information">
      <CollectionBar agent={agent} snapshot={snapshot ?? null} />
      {error && <div className="error-banner" role="alert"><CircleAlert size={16} aria-hidden="true" /><span>{error}</span></div>}
      {snapshot === undefined ? (
        !error && <div className="empty-state compact"><LoaderCircle size={22} className="spin" aria-hidden="true" /><strong>Loading…</strong></div>
      ) : snapshot === null ? (
        <div className="empty-state compact">
          <ServerOff size={22} aria-hidden="true" />
          <strong>Nothing collected yet</strong>
          <span>{agent.connected
            ? "The device sends its hardware and software within a minute of connecting. Refresh asks for it now."
            : "The device sends its hardware and software when it next connects."}</span>
        </div>
      ) : (
        <>
          {snapshot.problems.length > 0 && <Problems problems={snapshot.problems} />}
          {tab === "hardware" && <HardwarePanel snapshot={snapshot} />}
          {tab === "software" && <SoftwarePanel agent={agent} receivedAt={snapshot.received_at} count={snapshot.application_count} />}
          {tab === "network" && <NetworkPanel snapshot={snapshot} />}
        </>
      )}
    </section>
  );
}

function CollectionBar({ agent, snapshot }: { agent: Agent; snapshot: Snapshot | null }) {
  const { authorizedFetch } = useWorkspace();
  const receivedAt = agent.system?.received_at ?? snapshot?.received_at ?? null;
  // The snapshot time a refresh is waiting to see replaced.
  const [waiting, setWaiting] = useState<{ since: number | null } | null>(null);
  const [notice, setNotice] = useState<string | null>(null);
  const [, setTick] = useState(0);

  if (waiting && receivedAt !== waiting.since) setWaiting(null);

  useEffect(() => {
    if (!waiting) return;
    const timer = window.setTimeout(() => {
      setWaiting(null);
      setNotice("The device hasn’t sent its system information yet. It may still be collecting; this page updates when it arrives.");
    }, REFRESH_PATIENCE_MS);
    return () => window.clearTimeout(timer);
  }, [waiting]);

  // "Collected 3 minutes ago" stays current.
  useEffect(() => {
    const timer = window.setInterval(() => setTick((tick) => tick + 1), 30_000);
    return () => window.clearInterval(timer);
  }, []);

  const refresh = async () => {
    setNotice(null);
    setWaiting({ since: receivedAt });
    try {
      await requestRefresh(authorizedFetch, agent.id);
    } catch (error) {
      setWaiting(null);
      if (!(error instanceof AuthenticationRequired)) setNotice(errorText(error, "The device could not be asked for its system information."));
    }
  };

  return (
    <div className="system-bar">
      <p className="status-line">
        {receivedAt
          ? <span title={snapshot ? `Collected ${formatDateTime(snapshot.collected_at)} by the device’s clock` : undefined}>Collected {formatRelative(receivedAt)}</span>
          : <span>Not collected yet</span>}
      </p>
      <button
        type="button"
        className="secondary-button"
        onClick={() => void refresh()}
        disabled={!agent.connected || waiting !== null}
        title={agent.connected ? "Ask the device for its hardware and software now" : "The device is offline"}
      >
        <RefreshCw size={15} aria-hidden="true" className={waiting ? "spin" : undefined} />
        {waiting ? "Collecting…" : "Refresh"}
      </button>
      {notice && <p className="system-notice" role="status">{notice}</p>}
    </div>
  );
}

function Problems({ problems }: { problems: string[] }) {
  return (
    <details className="system-problems">
      <summary><TriangleAlert size={15} aria-hidden="true" />{problems.length === 1 ? "1 part couldn’t be collected" : `${problems.length} parts couldn’t be collected`}</summary>
      <ul>{problems.map((problem) => <li key={problem}>{problem}</li>)}</ul>
    </details>
  );
}

// A card of labelled facts; empty values are left out.
function Facts({ title, rows }: { title: string; rows: [string, ReactNode][] }) {
  const shown = rows.filter(([, value]) => value !== null && value !== undefined && value !== "");
  return (
    <section className="fact-card">
      <h3>{title}</h3>
      {shown.length ? (
        <dl>
          {shown.map(([label, value]) => (
            <div key={label}><dt>{label}</dt><dd>{value}</dd></div>
          ))}
        </dl>
      ) : <p className="field-help">Not reported.</p>}
    </section>
  );
}

function HardwarePanel({ snapshot }: { snapshot: Snapshot }) {
  const { os, hardware, users } = snapshot;
  const processor = hardware.processor;
  return (
    <>
      <div className="fact-grid">
        <Facts title="System" rows={[
          ["Manufacturer", hardware.manufacturer],
          ["Model", hardware.model],
          ["Serial number", hardware.serial_number && <span className="mono">{hardware.serial_number}</span>],
          ["Firmware", hardware.firmware_version],
        ]} />
        <Facts title="Processor and memory" rows={[
          ["Processor", processor?.name],
          ["Cores", processor && `${processor.cores} cores, ${processor.threads} threads`],
          ["Memory", hardware.memory_bytes ? formatBytes(hardware.memory_bytes) : null],
        ]} />
        <Facts title="Operating system" rows={[
          ["System", describeOs(os)],
          ["Kernel", os.kernel],
          ["Architecture", os.architecture],
          ["Installed", os.installed_at_unix_ms ? formatDateTime(os.installed_at_unix_ms) : null],
          ["Last boot", os.boot_time_unix_ms ? formatDateTime(os.boot_time_unix_ms) : null],
          ["Time zone", os.time_zone],
          ["Updates", os.package_managers.join(", ")],
        ]} />
      </div>

      <section className="agent-panel">
        <div className="panel-header"><div><h2>Disks</h2></div></div>
        {hardware.disks.length ? (
          <div className="table-scroll">
            <table className="data-table">
              <thead><tr><th scope="col">Disk</th><th scope="col">Model</th><th scope="col">Size</th><th scope="col">Type</th><th scope="col">Interface</th></tr></thead>
              <tbody>
                {hardware.disks.map((disk) => (
                  <tr key={disk.name}>
                    <td className="mono">{disk.name}</td>
                    <td>{disk.model ?? "—"}</td>
                    <td className="numeric">{formatBytes(disk.size_bytes)}</td>
                    <td>{disk.media === "ssd" ? "SSD" : disk.media === "hdd" ? "Hard disk" : "—"}</td>
                    <td>{disk.interface ?? "—"}</td>
                  </tr>
                ))}
              </tbody>
            </table>
          </div>
        ) : <p className="table-message field-help">No physical disks were reported.</p>}
      </section>

      <section className="agent-panel">
        <div className="panel-header"><div><h2>Signed-in users</h2></div></div>
        {users.length ? (
          <div className="table-scroll">
            <table className="data-table">
              <thead><tr><th scope="col">User</th><th scope="col">Session</th><th scope="col">From</th><th scope="col">Since</th></tr></thead>
              <tbody>
                {users.map((user, index) => (
                  <tr key={`${user.name}-${user.session}-${index}`}>
                    <td>{user.name}</td>
                    <td>{user.session ?? "—"}</td>
                    <td>{user.remote_host ?? "This computer"}</td>
                    <td>{user.since_unix_ms ? formatDateTime(user.since_unix_ms) : "—"}</td>
                  </tr>
                ))}
              </tbody>
            </table>
          </div>
        ) : <p className="table-message field-help">Nobody was signed in.</p>}
      </section>
    </>
  );
}

function NetworkPanel({ snapshot }: { snapshot: Snapshot }) {
  const { network } = snapshot;
  const adapters = sortAdapters(network.adapters);
  return (
    <>
      <div className="fact-grid">
        <Facts title="Identity" rows={[
          ["Host name", network.hostname],
          ["Full name", network.fqdn],
          ["Domain", network.domain],
          ["Workgroup", network.workgroup],
          ["Public address", snapshot.public_ip && <span className="mono">{snapshot.public_ip}</span>],
          ["DNS servers", network.dns_servers.length ? <span className="mono">{network.dns_servers.join(", ")}</span> : null],
        ]} />
      </div>
      <section className="agent-panel">
        <div className="panel-header"><div><h2>Adapters</h2><span>{adapters.length}</span></div></div>
        {adapters.length ? (
          <div className="table-scroll">
            <table className="data-table adapter-table">
              <thead><tr><th scope="col">Adapter</th><th scope="col">MAC address</th><th scope="col">Addresses</th><th scope="col">Gateway</th><th scope="col">DNS</th></tr></thead>
              <tbody>
                {adapters.map((adapter) => (
                  <tr key={adapter.name} className={adapter.up ? undefined : "muted-row"}>
                    <td>
                      <strong>{adapter.name}</strong>
                      <span>{[ADAPTER_KIND_LABELS[adapter.kind] ?? adapter.kind, adapter.up ? formatLinkSpeed(adapter.speed_bits_per_second) ?? "Up" : "Down", adapter.description].filter(Boolean).join(" · ")}</span>
                    </td>
                    <td className="mono">{adapter.mac_address ?? "—"}</td>
                    <td className="mono">{adapter.addresses.length ? adapter.addresses.map((entry) => <div key={entry.address}>{entry.address}/{entry.prefix_length}</div>) : "—"}</td>
                    <td className="mono">{adapter.gateways.length ? adapter.gateways.map((gateway) => <div key={gateway}>{gateway}</div>) : "—"}</td>
                    <td className="mono">{adapter.dns_servers.length ? adapter.dns_servers.map((server) => <div key={server}>{server}</div>) : "—"}</td>
                  </tr>
                ))}
              </tbody>
            </table>
          </div>
        ) : <p className="table-message field-help">No network adapters were reported.</p>}
      </section>
    </>
  );
}

const COLUMNS: { key: ApplicationSortKey; label: string }[] = [
  { key: "name", label: "Name" },
  { key: "version", label: "Version" },
  { key: "publisher", label: "Publisher" },
  { key: "installed_at", label: "Installed" },
  { key: "source", label: "Source" },
];

const CHANGE_LABELS: Record<ApplicationChange["change"], string> = {
  installed: "Installed",
  removed: "Removed",
  updated: "Updated",
};

function SoftwarePanel({ agent, receivedAt, count }: { agent: Agent; receivedAt: number; count: number }) {
  const { authorizedFetch } = useWorkspace();
  // The fleet's software search links here with the application's name.
  const [searchParams] = useSearchParams();
  const [query, setQuery] = useState(() => searchParams.get("q") ?? "");
  const [sort, setSort] = useState<ApplicationSort>(DEFAULT_APPLICATION_SORT);
  const applications = useLoaded<Application[]>(
    () => fetchApplications(authorizedFetch, agent.id),
    [authorizedFetch, agent.id, receivedAt],
    "The device’s applications could not be loaded.",
  );
  const changes = useLoaded<ApplicationChange[]>(
    () => fetchChanges(authorizedFetch, agent.id),
    [authorizedFetch, agent.id, receivedAt],
    "The device’s application changes could not be loaded.",
  );
  const visible = useMemo(() => visibleApplications(applications.data ?? [], query, sort), [applications.data, query, sort]);

  return (
    <>
      <section className="agent-panel">
        <div className="table-toolbar">
          <label className="search-field"><Search size={16} aria-hidden="true" /><input type="search" aria-label="Search applications" value={query} onChange={(event) => setQuery(event.target.value)} placeholder="Search applications" /></label>
          <span className="field-help" role="status">{query.trim() ? `${visible.length} of ${count}` : `${count} applications`}</span>
        </div>
        {applications.error && <p className="table-message form-error" role="alert"><CircleAlert size={16} aria-hidden="true" />{applications.error}</p>}
        {applications.data === undefined ? (
          !applications.error && <div className="empty-state"><LoaderCircle size={22} className="spin" aria-hidden="true" /><strong>Loading applications…</strong></div>
        ) : visible.length === 0 ? (
          <div className="empty-state">
            <PackageSearch size={22} aria-hidden="true" />
            <strong>{query.trim() ? "No matching applications" : "No applications reported"}</strong>
            {query.trim() && <button type="button" className="secondary-button" onClick={() => setQuery("")}>Clear search</button>}
          </div>
        ) : (
          <div className="table-scroll software-scroll">
            <table className="data-table software-table">
              <thead>
                <tr>
                  {COLUMNS.map((column) => (
                    <th key={column.key} scope="col" aria-sort={sort.key === column.key ? sort.direction : undefined}>
                      <button type="button" className="sort-button" onClick={() => setSort(nextSort(sort, column.key))}>
                        {column.label}
                        {sort.key === column.key && (sort.direction === "ascending" ? <ArrowUp size={13} aria-hidden="true" /> : <ArrowDown size={13} aria-hidden="true" />)}
                      </button>
                    </th>
                  ))}
                </tr>
              </thead>
              <tbody>
                {visible.map((application, index) => (
                  <tr key={`${application.name}\u0000${application.version}\u0000${application.source}\u0000${application.architecture}\u0000${index}`}>
                    <td className="software-name">{application.name}{application.architecture && <span className="badge badge-muted">{application.architecture}</span>}</td>
                    <td className="mono">{application.version ?? "—"}</td>
                    <td>{application.publisher ?? "—"}</td>
                    <td className="numeric">{formatDate(application.installed_at)}</td>
                    <td>{sourceLabel(application.source)}</td>
                  </tr>
                ))}
              </tbody>
            </table>
          </div>
        )}
      </section>

      <section className="agent-panel">
        <div className="panel-header"><div><h2>Recent changes</h2></div></div>
        {changes.error && <p className="table-message form-error" role="alert"><CircleAlert size={16} aria-hidden="true" />{changes.error}</p>}
        {changes.data && changes.data.length > 0 ? (
          <div className="table-scroll software-scroll">
            <table className="data-table">
              <thead><tr><th scope="col">Seen</th><th scope="col">Change</th><th scope="col">Application</th><th scope="col">Version</th></tr></thead>
              <tbody>
                {changes.data.map((change, index) => (
                  <tr key={`${change.observed_at}-${change.name}-${index}`}>
                    <td className="numeric">{formatDateTime(change.observed_at)}</td>
                    <td><span className={`badge change-${change.change}`}>{CHANGE_LABELS[change.change] ?? change.change}</span></td>
                    <td>{change.name}</td>
                    <td className="mono">{change.change === "updated" ? `${change.previous_version ?? "—"} → ${change.version ?? "—"}` : change.version ?? "—"}</td>
                  </tr>
                ))}
              </tbody>
            </table>
          </div>
        ) : changes.data && <p className="table-message field-help">No changes yet. Applications installed, removed or updated after the device’s first snapshot are listed here for 90 days.</p>}
      </section>
    </>
  );
}
