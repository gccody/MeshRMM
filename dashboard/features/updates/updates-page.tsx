import { CircleAlert, Download, LoaderCircle, RefreshCw, Search, SearchX, ShieldCheck, X } from "lucide-react";
import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { Link, useNavigate, useSearchParams } from "react-router";
import { formatDateTime, formatRelative } from "../../lib/format";
import { AuthenticationRequired, errorText } from "../../lib/http";
import { useResource } from "../../lib/use-resource";
import { PLATFORM_LABELS } from "../agents/types";
import { HeaderActions } from "../workspace/header-actions";
import { useWorkspace } from "../workspace/workspace-context";
import { BulkInstallDialog } from "./bulk-install-dialog";
import {
  type DeviceUpdateSummary,
  FLEET_STATUS_LABELS,
  type FleetFilters,
  type FleetStatusFilter,
  NO_COMPANY_FILTER,
  RUN_STATUS_LABELS,
  applySummaryUpdate,
  fleetFilterParams,
  fleetTotals,
  formatAge,
  isStale,
  managerLabel,
  matchesFleetFilters,
  parseFleetFilters,
  runTone,
  sortFleet,
} from "./model";
import { fetchFleet, startUpdateJob } from "./updates-api";
import { useUpdateEvents } from "./use-update-feed";

// While a device scans or installs, the list is read again this often, in
// case an event was missed.
const POLL_INTERVAL_MS = 15_000;

const TILES: { status: Exclude<FleetStatusFilter, "" | "pending">; label: string; empty: string }[] = [
  { status: "security", label: "Security updates pending", empty: "No device" },
  { status: "failed", label: "Last install failed", empty: "No device" },
  { status: "reboot", label: "Restart pending", empty: "No device" },
  { status: "stale", label: "Not scanned in 3 days", empty: "Every device" },
];

// `/updates`: every device's pending updates at a glance, filtered by
// company, operating system and state, with installs on many devices.
export function UpdatesPage() {
  const { authorizedFetch, inventory, can } = useWorkspace();
  const navigate = useNavigate();
  const [searchParams, setSearchParams] = useSearchParams();
  const filters = parseFleetFilters(searchParams);
  const load = useCallback(() => fetchFleet(authorizedFetch), [authorizedFetch]);
  const { data, error, reload, setData } = useResource(load, "The devices’ updates could not be loaded.");
  const [chosen, setChosen] = useState<ReadonlySet<string>>(() => new Set());
  const [dialog, setDialog] = useState(false);
  const [scanning, setScanning] = useState(false);
  const [actionError, setActionError] = useState<string | null>(null);
  const opener = useRef<HTMLElement | null>(null);
  const [now, setNow] = useState(() => Date.now());
  const canInstall = can("updates.install");

  const choose = (change: Partial<FleetFilters>) => setSearchParams(fleetFilterParams({ ...filters, ...change }), { replace: true });

  useUpdateEvents((event) => {
    if (event.type === "updates_resync") reload();
    else if (event.type === "device_updates") setData((current) => applySummaryUpdate(current, event.device));
  });

  // Ages and "not scanned recently" stay current.
  useEffect(() => {
    const timer = window.setInterval(() => setNow(Date.now()), 60_000);
    return () => window.clearInterval(timer);
  }, []);

  const busy = Boolean(data?.some((device) => device.scanning || device.installing));
  useEffect(() => {
    if (!busy) return;
    const timer = window.setInterval(reload, POLL_INTERVAL_MS);
    return () => window.clearInterval(timer);
  }, [busy, reload]);

  // The live inventory knows best which devices are online, and which exist.
  const devices = useMemo(() => {
    if (!data) return null;
    const agents = new Map(inventory.agents.map((agent) => [agent.id, agent]));
    return sortFleet(data
      .filter((device) => !inventory.hasData || agents.has(device.device_id))
      .map((device) => {
        const agent = agents.get(device.device_id);
        return agent ? { ...device, device_name: agent.name, company_id: agent.company_id ?? null, connected: agent.connected } : device;
      }));
  }, [data, inventory.agents, inventory.hasData]);
  const shown = (devices ?? []).filter((device) => matchesFleetFilters(device, filters, now));
  const totals = useMemo(() => fleetTotals(devices ?? [], now), [devices, now]);
  const chosenDevices = (devices ?? []).filter((device) => chosen.has(device.device_id));
  const allShownChosen = shown.length > 0 && shown.every((device) => chosen.has(device.device_id));
  const filtered = Boolean(filters.query.trim() || filters.company || filters.platform || filters.status);

  const toggle = (id: string, checked: boolean) => setChosen((current) => {
    const next = new Set(current);
    if (checked) next.add(id);
    else next.delete(id);
    return next;
  });
  const chooseShown = (checked: boolean) => setChosen((current) => {
    const next = new Set(current);
    for (const device of shown) {
      if (checked) next.add(device.device_id);
      else next.delete(device.device_id);
    }
    return next;
  });

  const scan = async () => {
    setScanning(true);
    setActionError(null);
    try {
      const job = await startUpdateJob(authorizedFetch, chosenDevices.map((device) => device.device_id), { action: "scan" }, { offline: "skip", wait_hours: 24 });
      void navigate(`/job?id=${encodeURIComponent(job.id)}`);
    } catch (failure) {
      if (!(failure instanceof AuthenticationRequired)) setActionError(errorText(failure, "The scan could not be started."));
      setScanning(false);
    }
  };

  return (
    <>
      <HeaderActions>
        <button type="button" className="icon-button" onClick={reload} aria-label="Refresh" title="Refresh"><RefreshCw size={16} /></button>
      </HeaderActions>
      {actionError && (
        <div className="error-banner" role="alert">
          <CircleAlert size={17} aria-hidden="true" /><span>{actionError}</span>
          <button onClick={() => setActionError(null)} aria-label="Dismiss"><X size={16} /></button>
        </div>
      )}
      {devices && (
        <div className="update-tiles" role="group" aria-label="Show devices">
          {TILES.map((tile) => {
            const count = totals[tile.status];
            return (
              <button key={tile.status} type="button" aria-pressed={filters.status === tile.status} className={`update-tile${count > 0 && tile.status !== "stale" ? " attention" : ""}`} onClick={() => choose({ status: filters.status === tile.status ? "" : tile.status })}>
                <span>{tile.label}</span>
                <strong>{count}</strong>
                <small>{count === 0 ? tile.empty : `of ${totals.devices} ${totals.devices === 1 ? "device" : "devices"}`}</small>
              </button>
            );
          })}
        </div>
      )}
      {chosenDevices.length > 0 && (
        <div className="selection-bar" role="region" aria-label="Chosen devices">
          <strong>{chosenDevices.length} {chosenDevices.length === 1 ? "device" : "devices"} chosen</strong>
          <div className="selection-actions">
            <button type="button" className="secondary-button" disabled={scanning} onClick={() => void scan()}>{scanning ? <LoaderCircle size={15} className="spin" /> : <RefreshCw size={15} />} Scan now</button>
            {canInstall && <button type="button" className="primary-button" onClick={(event) => { opener.current = event.currentTarget; setDialog(true); }} aria-haspopup="dialog"><Download size={15} /> Install updates…</button>}
            <button type="button" className="secondary-button" onClick={() => setChosen(new Set())}>Clear</button>
          </div>
        </div>
      )}
      <section className="agent-panel">
        <div className="table-toolbar">
          <label className="search-field software-search">
            <Search size={16} aria-hidden="true" />
            <input type="search" aria-label="Search devices" value={filters.query} onChange={(event) => choose({ query: event.target.value })} placeholder="Search devices" />
          </label>
          <div className="toolbar-filters">
            <select aria-label="Company" value={filters.company} onChange={(event) => choose({ company: event.target.value })}>
              <option value="">Every company</option>
              <option value={NO_COMPANY_FILTER}>No company</option>
              {inventory.companies.map((company) => <option key={company.id} value={company.id}>{company.name}</option>)}
            </select>
            <select aria-label="Operating system" value={filters.platform} onChange={(event) => choose({ platform: event.target.value as FleetFilters["platform"] })}>
              <option value="">Every system</option>
              {(["windows", "macos", "linux"] as const).map((platform) => <option key={platform} value={platform}>{PLATFORM_LABELS[platform]}</option>)}
            </select>
            <select aria-label="State" value={filters.status} onChange={(event) => choose({ status: event.target.value as FleetStatusFilter })}>
              <option value="">Any state</option>
              {(Object.keys(FLEET_STATUS_LABELS) as Exclude<FleetStatusFilter, "">[]).map((status) => <option key={status} value={status}>{FLEET_STATUS_LABELS[status]}</option>)}
            </select>
          </div>
        </div>
        {!devices ? (
          error
            ? <div className="empty-state"><CircleAlert size={22} aria-hidden="true" /><strong>{error}</strong><button type="button" className="secondary-button" onClick={reload}><RefreshCw size={16} /> Try again</button></div>
            : <div className="empty-state"><LoaderCircle size={22} className="spin" aria-hidden="true" /><strong>Loading updates…</strong></div>
        ) : shown.length === 0 ? (
          <div className="empty-state">
            {filtered ? <SearchX size={22} aria-hidden="true" /> : <ShieldCheck size={22} aria-hidden="true" />}
            <strong>{filtered ? "No devices match" : "No devices yet"}</strong>
            <span>{filtered ? "Change the filters to see more devices." : "Enrolled devices look for operating system updates on the server’s schedule and list them here."}</span>
            {filtered && <button type="button" className="secondary-button" onClick={() => setSearchParams(new URLSearchParams(), { replace: true })}>Clear filters</button>}
          </div>
        ) : (
          <>
            {error && <p className="table-message form-error" role="alert"><CircleAlert size={16} aria-hidden="true" />{error}</p>}
            <div className="table-scroll">
              <table className="data-table fleet-updates" aria-label="Devices’ updates">
                <thead>
                  <tr>
                    <th scope="col" className="select-cell"><input type="checkbox" aria-label={`Choose all ${shown.length} devices shown`} checked={allShownChosen} onChange={(event) => chooseShown(event.target.checked)} /></th>
                    <th scope="col">Device</th>
                    <th scope="col">Pending</th>
                    <th scope="col">Oldest</th>
                    <th scope="col">Last scan</th>
                    <th scope="col">Restart</th>
                    <th scope="col">Last install</th>
                  </tr>
                </thead>
                <tbody>
                  {shown.map((device) => <FleetRow key={device.device_id} device={device} now={now} chosen={chosen.has(device.device_id)} onToggle={(checked) => toggle(device.device_id, checked)} />)}
                </tbody>
              </table>
            </div>
            <p className="panel-footer field-help">{shown.length === devices.length ? `${devices.length} ${devices.length === 1 ? "device" : "devices"}` : `${shown.length} of ${devices.length} devices`}</p>
          </>
        )}
      </section>
      {dialog && <BulkInstallDialog devices={chosenDevices} returnFocus={opener} onClose={() => setDialog(false)} />}
    </>
  );
}

function FleetRow({ device, now, chosen, onToggle }: { device: DeviceUpdateSummary; now: number; chosen: boolean; onToggle: (checked: boolean) => void }) {
  const link = `/device?${new URLSearchParams({ id: device.device_id, tab: "updates" })}`;
  const stale = isStale(device, now);
  return (
    <tr className={chosen ? "selected-row" : undefined}>
      <td className="select-cell"><input type="checkbox" aria-label={`Choose ${device.device_name}`} checked={chosen} onChange={(event) => onToggle(event.target.checked)} /></td>
      <td>
        <strong><Link to={link}>{device.device_name}</Link></strong>
        <span>{[PLATFORM_LABELS[device.platform], managerLabel(device.manager), device.connected ? null : "Offline"].filter(Boolean).join(" · ")}</span>
      </td>
      <td>
        {!device.supported ? <span>Not supported</span>
          : device.scanned_at === null ? <span>—</span>
          : device.pending === 0 ? <span className="run-pill run-pill-success">Up to date</span>
          : <>
            <strong className="numeric-strong">{device.pending}</strong>
            {device.security > 0 && <span className="badge update-category category-security">{device.security} security</span>}
            {device.critical > 0 && <span className="badge update-category category-critical">{device.critical} critical</span>}
          </>}
        {device.installing && <span className="run-pill run-pill-pending">Installing</span>}
        {device.scanning && !device.installing && <span className="run-pill run-pill-pending">Scanning</span>}
      </td>
      <td className="numeric">{device.oldest_pending_at ? formatAge(device.oldest_pending_at, now) : "—"}</td>
      <td>
        {device.received_at
          ? <span className={stale ? "stale-text" : undefined} title={device.scanned_at ? `Scanned ${formatDateTime(device.scanned_at)} by the device’s clock` : undefined}>{formatRelative(device.received_at, now)}</span>
          : <span>Never</span>}
        {device.scan_error && <span className="error-text" title={device.scan_error}>Failed: {device.scan_error}</span>}
      </td>
      <td>{device.reboot_pending ? <span className="run-pill run-pill-problem" title={device.reboot_pending_since ? `Since ${formatDateTime(device.reboot_pending_since)}` : undefined}>Pending{device.reboot_pending_since ? ` ${formatAge(device.reboot_pending_since, now)}` : ""}</span> : <span>—</span>}</td>
      <td>
        {device.last_install ? (
          <>
            <span className={`run-pill run-pill-${runTone(device.last_install.status)}`}>{RUN_STATUS_LABELS[device.last_install.status]}</span>
            <span>{formatRelative(device.last_install.completed_at ?? device.last_install.created_at, now)}</span>
          </>
        ) : <span>—</span>}
      </td>
    </tr>
  );
}
