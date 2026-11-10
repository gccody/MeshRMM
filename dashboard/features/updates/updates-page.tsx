import { CircleAlert, Download, LoaderCircle, RefreshCw, Search, SearchX, ShieldCheck, X } from "lucide-react";
import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { Link, useNavigate, useSearchParams } from "react-router";
import { formatDateTime, formatRelative } from "../../lib/format";
import { AuthenticationRequired, errorText } from "../../lib/http";
import { remainingItems } from "../../lib/paged-list";
import { ShowMore } from "../../lib/show-more";
import { useNow } from "../../lib/use-now";
import { useSettled } from "../../lib/use-settled";
import { PLATFORM_LABELS } from "../agents/types";
import { HeaderActions } from "../workspace/header-actions";
import { useWorkspace } from "../workspace/workspace-context";
import { useCompanies, useDeviceLookup, useTags } from "../agents/use-devices";
import { BulkInstallDialog } from "./bulk-install-dialog";
import {
  type ChosenDevices,
  type DeviceUpdateSummary,
  FLEET_PAGE_SIZE,
  FLEET_STATUS_LABELS,
  type FleetAnswer,
  type FleetFilters,
  type FleetList,
  type FleetStatusFilter,
  NO_COMPANY_FILTER,
  RUN_STATUS_LABELS,
  applyFleetAnswer,
  chooseDevices,
  countsRefreshDelay,
  fleetFilterParams,
  fleetKey,
  formatAge,
  isStale,
  managerLabel,
  parseFleetFilters,
  patchFleetRow,
  refreshChosen,
  runTone,
} from "./model";
import { fetchFleetPage, startUpdateJob } from "./updates-api";
import { useUpdateEvents } from "./use-update-feed";

const TILES: { status: Exclude<FleetStatusFilter, "" | "pending">; label: string; empty: string }[] = [
  { status: "security", label: "Security updates pending", empty: "No device" },
  { status: "failed", label: "Last install failed", empty: "No device" },
  { status: "reboot", label: "Restart pending", empty: "No device" },
  { status: "stale", label: "Not scanned in 3 days", empty: "Every device" },
];

// `/updates`: the devices' pending updates at a glance, most urgent first,
// filtered by company, tag, operating system and state, with installs on
// many devices. The server filters, orders and counts the fleet and sends
// it a page at a time; events from the socket keep the loaded rows current.
export function UpdatesPage() {
  const { authorizedFetch, inventory, can } = useWorkspace();
  const fleet = useDeviceLookup(inventory.devices);
  const companies = useCompanies(inventory.devices);
  const tags = useTags(inventory.devices);
  const navigate = useNavigate();
  const [searchParams, setSearchParams] = useSearchParams();
  const filters = useMemo(() => parseFleetFilters(searchParams), [searchParams]);
  const query = useSettled(filters.query.trim());
  const { company, tag, platform, status } = filters;
  const requested = useMemo<FleetFilters>(() => ({ query, company, tag, platform, status }), [query, company, tag, platform, status]);
  const key = fleetKey(requested);
  const [list, setList] = useState<FleetList | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [attempt, setAttempt] = useState(0);
  const [loadingMore, setLoadingMore] = useState(false);
  const [chosen, setChosen] = useState<ChosenDevices>(() => new Map());
  const [dialog, setDialog] = useState(false);
  const [scanning, setScanning] = useState(false);
  const [actionError, setActionError] = useState<string | null>(null);
  const opener = useRef<HTMLElement | null>(null);
  const now = useNow(60_000);
  const canInstall = can("updates.install");
  // Requests are numbered as they're sent, so an older one's answer never
  // replaces a newer one's.
  const tickets = useRef(0);
  const wanted = useRef(key);
  const countsAt = useRef(0);
  const countsTimer = useRef<number | undefined>(undefined);

  const choose = (change: Partial<FleetFilters>) => setSearchParams(fleetFilterParams({ ...filters, ...change }), { replace: true });

  useEffect(() => {
    wanted.current = key;
  }, [key]);

  // Asks for the filters' page from `offset`, or for the counts alone.
  const ask = useCallback(async (kind: FleetAnswer["kind"], offset: number) => {
    const ticket = ++tickets.current;
    try {
      const page = await fetchFleetPage(authorizedFetch, requested, offset, kind === "counts" ? 0 : FLEET_PAGE_SIZE);
      countsAt.current = Date.now();
      const current = wanted.current;
      const answer: FleetAnswer = kind === "counts" ? { kind, key, ticket, page } : { kind, key, ticket, offset, page };
      setList((list) => applyFleetAnswer(list, answer, current));
      if (kind === "page" && key === current) setError(null);
    } catch (failure) {
      if (kind === "page" && key === wanted.current && !(failure instanceof AuthenticationRequired)) {
        setError(errorText(failure, offset ? "More devices could not be loaded." : "The devices’ updates could not be loaded."));
      }
    }
  }, [authorizedFetch, requested, key]);
  const latestAsk = useRef(ask);
  useEffect(() => {
    latestAsk.current = ask;
  }, [ask]);

  // The first page, again whenever the filters change or a reload is due.
  useEffect(() => {
    void ask("page", 0);
  }, [ask, attempt]);

  const reload = () => setAttempt((count) => count + 1);

  // While changes keep arriving, the tiles' counts are read again at most
  // once every 10 seconds.
  const refreshCounts = () => {
    if (countsTimer.current !== undefined) return;
    countsTimer.current = window.setTimeout(() => {
      countsTimer.current = undefined;
      void latestAsk.current("counts", 0);
    }, countsRefreshDelay(countsAt.current, Date.now()));
  };
  useEffect(() => {
    const timer = countsTimer;
    return () => window.clearTimeout(timer.current);
  }, []);

  useUpdateEvents((event) => {
    if (event.type === "updates_resync") reload();
    else if (event.type === "device_updates") {
      setList((current) => patchFleetRow(current, event.device));
      setChosen((current) => refreshChosen(current, [event.device]));
      refreshCounts();
    }
  });

  // The live inventory knows best which devices exist, their names,
  // companies and tags, and which are online.
  const live = useCallback((device: DeviceUpdateSummary): DeviceUpdateSummary => {
    const agent = fleet.get(device.device_id);
    return agent ? { ...device, device_name: agent.name, company_id: agent.company_id ?? null, tag_ids: agent.tag_ids ?? [], connected: agent.connected } : device;
  }, [fleet]);
  const present = useCallback((device: DeviceUpdateSummary) => !inventory.hasData || Boolean(fleet.get(device.device_id)), [fleet, inventory.hasData]);
  const devices = useMemo(() => (list ? list.items.filter(present).map(live) : null), [list, present, live]);
  // A chosen device keeps the summary it was chosen with until a newer one
  // is loaded or heard.
  const chosenDevices = useMemo(() => {
    const loaded = new Map((list?.items ?? []).map((device) => [device.device_id, device]));
    return [...chosen.values()].map((device) => loaded.get(device.device_id) ?? device).filter(present).map(live);
  }, [chosen, list, present, live]);
  const shown = devices ?? [];
  const allShownChosen = shown.length > 0 && shown.every((device) => chosen.has(device.device_id));
  const filtered = Boolean(filters.query.trim() || filters.company || filters.tag || filters.platform || filters.status);
  const remaining = remainingItems(list);

  const toggle = (device: DeviceUpdateSummary, checked: boolean) => setChosen((current) => chooseDevices(current, [device], checked));
  const chooseShown = (checked: boolean) => setChosen((current) => chooseDevices(current, shown, checked));

  const showMore = async () => {
    if (!list) return;
    setLoadingMore(true);
    await ask("page", list.items.length);
    setLoadingMore(false);
  };

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

  const totals = list?.totals;
  const deviceCount = (count: number) => `${count} ${count === 1 ? "device" : "devices"}`;

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
      {totals && (
        <div className="update-tiles" role="group" aria-label="Show devices">
          {TILES.map((tile) => {
            const count = totals[tile.status];
            return (
              <button key={tile.status} type="button" aria-pressed={filters.status === tile.status} className={`update-tile${count > 0 && tile.status !== "stale" ? " attention" : ""}`} onClick={() => choose({ status: filters.status === tile.status ? "" : tile.status })}>
                <span>{tile.label}</span>
                <strong>{count}</strong>
                <small>{count === 0 ? tile.empty : `of ${deviceCount(totals.devices)}`}</small>
              </button>
            );
          })}
        </div>
      )}
      {chosenDevices.length > 0 && (
        <div className="selection-bar" role="region" aria-label="Chosen devices">
          <strong>{deviceCount(chosenDevices.length)} chosen</strong>
          <div className="selection-actions">
            <button type="button" className="secondary-button" disabled={scanning} onClick={() => void scan()}>{scanning ? <LoaderCircle size={15} className="spin" /> : <RefreshCw size={15} />} Scan now</button>
            {canInstall && <button type="button" className="primary-button" onClick={(event) => { opener.current = event.currentTarget; setDialog(true); }} aria-haspopup="dialog"><Download size={15} /> Install updates…</button>}
            <button type="button" className="secondary-button" onClick={() => setChosen(new Map())}>Clear</button>
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
              {companies.map((company) => <option key={company.id} value={company.id}>{company.name}</option>)}
            </select>
            {(tags.length > 0 || filters.tag) && (
              <select aria-label="Tag" value={filters.tag} onChange={(event) => choose({ tag: event.target.value })}>
                <option value="">Every tag</option>
                {filters.tag && !tags.some((tag) => tag.id === filters.tag) && <option value={filters.tag}>Unknown tag</option>}
                {tags.map((tag) => <option key={tag.id} value={tag.id}>{tag.name}</option>)}
              </select>
            )}
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
        {!list || !devices ? (
          error
            ? <div className="empty-state"><CircleAlert size={22} aria-hidden="true" /><strong>{error}</strong><button type="button" className="secondary-button" onClick={reload}><RefreshCw size={16} /> Try again</button></div>
            : <div className="empty-state"><LoaderCircle size={22} className="spin" aria-hidden="true" /><strong>Loading updates…</strong></div>
        ) : shown.length === 0 && remaining === 0 ? (
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
              <table className="data-table fleet-updates" aria-label="Devices’ updates" aria-busy={list.key !== key}>
                <thead>
                  <tr>
                    <th scope="col" className="select-cell"><input type="checkbox" aria-label={`Choose the ${deviceCount(shown.length)} loaded`} title={`Choose the ${deviceCount(shown.length)} loaded`} checked={allShownChosen} disabled={shown.length === 0} onChange={(event) => chooseShown(event.target.checked)} /></th>
                    <th scope="col">Device</th>
                    <th scope="col">Pending</th>
                    <th scope="col">Oldest</th>
                    <th scope="col">Last scan</th>
                    <th scope="col">Restart</th>
                    <th scope="col">Last install</th>
                  </tr>
                </thead>
                <tbody>
                  {shown.map((device) => <FleetRow key={device.device_id} device={device} now={now} chosen={chosen.has(device.device_id)} onToggle={(checked) => toggle(device, checked)} />)}
                </tbody>
              </table>
            </div>
            <div className="panel-footer fleet-footer">
              <span className="field-help">
                {filtered ? `${list.matching} of ${deviceCount(list.totals.devices)}` : deviceCount(list.totals.devices)}
                {remaining > 0 && ` · ${list.items.length} loaded`}
              </span>
              <ShowMore remaining={remaining} pageSize={FLEET_PAGE_SIZE} loading={loadingMore} disabled={list.key !== key} onShowMore={() => void showMore()} />
            </div>
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
