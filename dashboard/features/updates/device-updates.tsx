import { CircleAlert, Download, LoaderCircle, PackageCheck, RefreshCw, RotateCcw, SearchX, ServerOff, ShieldAlert } from "lucide-react";
import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { formatDateTime, formatRelative } from "../../lib/format";
import { AuthenticationRequired, errorText } from "../../lib/http";
import { useResource } from "../../lib/use-resource";
import type { Agent } from "../agents/types";
import { formatBytes } from "../metrics/model";
import { useWorkspace } from "../workspace/workspace-context";
import { InstallDialog } from "./install-dialog";
import {
  CATEGORY_LABELS,
  type DeviceUpdates as DeviceUpdatesData,
  type PendingUpdate,
  SEVERITY_LABELS,
  type UpdateCategory,
  type UpdateRun,
  categoryCounts,
  formatAge,
  managerLabel,
  sortPendingUpdates,
  updateDetail,
} from "./model";
import { InstallHistory } from "./install-history";
import { fetchDeviceUpdates, requestScan } from "./updates-api";
import { useUpdateEvents } from "./use-update-feed";

// While a scan or install runs, the device's updates are read again this
// often, in case an event was missed.
const POLL_INTERVAL_MS = 15_000;
// Events come in bursts; the updates are read at most this often.
const EVENT_DEBOUNCE_MS = 1_000;

// `/device?id=…&tab=updates`: the device's pending operating system
// updates, scanning and installing them, and its install history.
export function DeviceUpdates({ agent }: { agent: Agent }) {
  const { authorizedFetch, can } = useWorkspace();
  const load = useCallback(() => fetchDeviceUpdates(authorizedFetch, agent.id), [authorizedFetch, agent.id]);
  const { data, error, reload, setData } = useResource(load, "The device’s updates could not be loaded.");
  // Bumped when an install finishes, so the history reads it.
  const [historyKey, setHistoryKey] = useState(0);
  const pending = useRef<number | undefined>(undefined);
  const busy = Boolean(data?.status.scanning || data?.status.installing);

  useEffect(() => () => window.clearTimeout(pending.current), []);

  const reloadSoon = () => {
    if (pending.current !== undefined) return;
    pending.current = window.setTimeout(() => {
      pending.current = undefined;
      reload();
    }, EVENT_DEBOUNCE_MS);
  };

  useUpdateEvents((event) => {
    if (event.type === "updates_resync") {
      reload();
      setHistoryKey((key) => key + 1);
      return;
    }
    if (event.type === "device_updates") {
      if (event.device.device_id === agent.id) reloadSoon();
      return;
    }
    const run = event.run;
    if (run.device_id !== agent.id) return;
    if (run.kind === "install") {
      // Progress shows at once; the list follows the install's scan.
      setData((current) => ({
        ...current,
        status: { ...current.status, installing: run.status === "pending" ? run : null },
      }));
      if (run.status !== "pending") setHistoryKey((key) => key + 1);
    }
    reloadSoon();
  });

  useEffect(() => {
    if (!busy) return;
    const timer = window.setInterval(reload, POLL_INTERVAL_MS);
    return () => window.clearInterval(timer);
  }, [busy, reload]);

  return (
    <section className="system-info updates-tab" aria-label="Updates">
      {!data ? (
        error
          ? <div className="empty-state compact"><CircleAlert size={22} aria-hidden="true" /><strong>{error}</strong><button type="button" className="secondary-button" onClick={reload}><RefreshCw size={16} /> Try again</button></div>
          : <div className="empty-state compact"><LoaderCircle size={22} className="spin" aria-hidden="true" /><strong>Loading updates…</strong></div>
      ) : (
        <>
          {error && <div className="error-banner" role="alert"><CircleAlert size={16} aria-hidden="true" /><span>{error}</span></div>}
          <UpdatesBody
            agent={agent}
            data={data}
            canInstall={can("updates.install")}
            onScanning={(run) => setData((current) => ({ ...current, status: { ...current.status, scanning: { run_id: run.id, created_at: run.created_at } } }))}
            onInstalling={(run) => {
              setData((current) => ({ ...current, status: { ...current.status, installing: run } }));
              setHistoryKey((key) => key + 1);
            }}
          />
        </>
      )}
      {data && <InstallHistory deviceId={agent.id} reloadKey={historyKey} />}
    </section>
  );
}

function UpdatesBody({ agent, data, canInstall, onScanning, onInstalling }: {
  agent: Agent;
  data: DeviceUpdatesData;
  canInstall: boolean;
  onScanning: (run: UpdateRun) => void;
  onInstalling: (run: UpdateRun) => void;
}) {
  const { status } = data;
  const updates = useMemo(() => sortPendingUpdates(data.updates), [data.updates]);
  const [category, setCategory] = useState<UpdateCategory | null>(null);
  const [chosen, setChosen] = useState<ReadonlySet<string>>(() => new Set());
  const [dialog, setDialog] = useState<"chosen" | "all" | null>(null);
  const opener = useRef<HTMLElement | null>(null);
  const shown = category ? updates.filter((update) => update.category === category) : updates;
  const counts = categoryCounts(updates);
  // Only updates still pending stay chosen.
  const chosenUpdates = updates.filter((update) => chosen.has(update.id));
  const selectable = canInstall && status.can_select;
  const installBlocked = !agent.connected ? "The device is offline." : status.installing ? "An install is running." : null;
  const allShownChosen = shown.length > 0 && shown.every((update) => chosen.has(update.id));
  if (category && !counts.some((entry) => entry.category === category)) setCategory(null);

  const toggle = (id: string, checked: boolean) => setChosen((current) => {
    const next = new Set(current);
    if (checked) next.add(id);
    else next.delete(id);
    return next;
  });
  const toggleShown = (checked: boolean) => setChosen((current) => {
    const next = new Set(current);
    for (const update of shown) {
      if (checked) next.add(update.id);
      else next.delete(update.id);
    }
    return next;
  });
  const open = (which: "chosen" | "all", button: HTMLElement) => {
    opener.current = button;
    setDialog(which);
  };

  return (
    <>
      <UpdateStatusBar agent={agent} data={data} onScanning={onScanning} />
      {status.installing && <InstallProgress run={status.installing} />}

      {status.scanned_at === null && status.scan_error === null ? (
        <div className="empty-state compact">
          <ServerOff size={22} aria-hidden="true" />
          <strong>{status.supported ? "Not scanned yet" : "No supported update manager"}</strong>
          <span>{!status.supported
            ? "The device has no update service or package manager MeshRMM supports. Windows Update, macOS Software Update, APT, DNF, YUM and pacman are."
            : agent.connected
              ? "The device looks for updates on the server’s schedule. Scan now asks it to look at once."
              : "The device looks for updates when it next connects."}</span>
        </div>
      ) : !status.supported ? (
        <div className="empty-state compact">
          <ServerOff size={22} aria-hidden="true" />
          <strong>No supported update manager</strong>
          <span>The device has no update service or package manager MeshRMM supports. Windows Update, macOS Software Update, APT, DNF, YUM and pacman are.</span>
        </div>
      ) : updates.length === 0 ? (
        status.scanned_at !== null && (
          <div className="empty-state compact">
            <PackageCheck size={22} aria-hidden="true" />
            <strong>Up to date</strong>
            <span>The last scan, {formatRelative(status.scanned_at)}, found no pending updates.{status.reboot_pending ? " A restart is still pending." : ""}</span>
          </div>
        )
      ) : (
        <section className="agent-panel">
          <div className="table-toolbar">
            <div className="outcome-chips" role="group" aria-label="Show updates">
              <button type="button" className="outcome-chip" aria-pressed={category === null} onClick={() => setCategory(null)}>All<em>{updates.length}</em></button>
              {counts.map((entry) => (
                <button key={entry.category} type="button" className={`outcome-chip${entry.category === "security" || entry.category === "critical" ? " outcome-problem" : ""}`} aria-pressed={category === entry.category} onClick={() => setCategory(category === entry.category ? null : entry.category)}>
                  {CATEGORY_LABELS[entry.category]}<em>{entry.count}</em>
                </button>
              ))}
            </div>
            {canInstall && (
              <div className="selection-actions">
                {selectable && (
                  <button type="button" className="secondary-button" disabled={!chosenUpdates.length || installBlocked !== null} title={installBlocked ?? undefined} onClick={(event) => open("chosen", event.currentTarget)} aria-haspopup="dialog">
                    <Download size={15} /> Install {chosenUpdates.length ? `${chosenUpdates.length} selected` : "selected"}
                  </button>
                )}
                <button type="button" className="primary-button" disabled={installBlocked !== null} title={installBlocked ?? undefined} onClick={(event) => open("all", event.currentTarget)} aria-haspopup="dialog">
                  <Download size={15} /> Install all
                </button>
              </div>
            )}
          </div>
          {!status.can_select && canInstall && <p className="table-message field-help">{managerLabel(status.manager)} upgrades every package at once, so updates can’t be installed one by one.</p>}
          <div className="table-scroll software-scroll">
            <table className="data-table update-table" aria-label="Pending updates">
              <thead>
                <tr>
                  {selectable && <th scope="col" className="select-cell"><input type="checkbox" aria-label="Select every update shown" checked={allShownChosen} onChange={(event) => toggleShown(event.target.checked)} /></th>}
                  <th scope="col">Update</th>
                  <th scope="col">Category</th>
                  <th scope="col" className="numeric">Size</th>
                  <th scope="col">Pending for</th>
                </tr>
              </thead>
              <tbody>
                {shown.map((update) => (
                  <UpdateRow key={update.id} update={update} selectable={selectable} chosen={chosen.has(update.id)} onToggle={(checked) => toggle(update.id, checked)} />
                ))}
              </tbody>
            </table>
          </div>
        </section>
      )}

      {dialog && (
        <InstallDialog
          deviceId={agent.id}
          deviceName={agent.name}
          chosen={dialog === "chosen" ? chosenUpdates : []}
          pendingCount={updates.length}
          returnFocus={opener}
          onClose={() => setDialog(null)}
          onStarted={(run) => {
            setDialog(null);
            setChosen(new Set());
            onInstalling(run);
          }}
        />
      )}
    </>
  );
}

function UpdateRow({ update, selectable, chosen, onToggle }: {
  update: PendingUpdate;
  selectable: boolean;
  chosen: boolean;
  onToggle: (checked: boolean) => void;
}) {
  const detail = updateDetail(update);
  return (
    <tr className={chosen ? "selected-row" : undefined}>
      {selectable && <td className="select-cell"><input type="checkbox" aria-label={`Select ${update.title}`} checked={chosen} onChange={(event) => onToggle(event.target.checked)} /></td>}
      <td className="update-title">
        <strong>{update.title}</strong>
        {detail && <span className="mono-detail">{detail}</span>}
      </td>
      <td>
        <span className={`badge update-category category-${update.category}`}>{CATEGORY_LABELS[update.category]}</span>
        {update.severity && <span className={`badge badge-muted severity-${update.severity}`}>{SEVERITY_LABELS[update.severity]}</span>}
        {update.reboot_required && <span className="badge badge-warn" title="Installing it may need a restart"><RotateCcw size={11} aria-hidden="true" />Restart</span>}
      </td>
      <td className="numeric">{update.size_bytes ? formatBytes(update.size_bytes) : "—"}</td>
      <td className="numeric" title={`First seen ${formatDateTime(update.first_seen_at)}${update.released_at ? `; released ${formatDateTime(update.released_at)}` : ""}`}>{formatAge(update.first_seen_at)}</td>
    </tr>
  );
}

// Which update manager the device has, when it last scanned, and Scan now.
function UpdateStatusBar({ agent, data, onScanning }: { agent: Agent; data: DeviceUpdatesData; onScanning: (run: UpdateRun) => void }) {
  const { authorizedFetch } = useWorkspace();
  const { status } = data;
  const [requesting, setRequesting] = useState(false);
  const [notice, setNotice] = useState<string | null>(null);
  const [, setTick] = useState(0);
  const scanning = requesting || status.scanning !== null;

  // "Scanned 3 minutes ago" stays current.
  useEffect(() => {
    const timer = window.setInterval(() => setTick((tick) => tick + 1), 30_000);
    return () => window.clearInterval(timer);
  }, []);

  const scan = async () => {
    setNotice(null);
    setRequesting(true);
    try {
      onScanning(await requestScan(authorizedFetch, agent.id));
    } catch (error) {
      if (!(error instanceof AuthenticationRequired)) setNotice(errorText(error, "The device could not be asked to scan for updates."));
    } finally {
      setRequesting(false);
    }
  };

  return (
    <>
      <div className="system-bar">
        <p className="status-line">
          {status.manager && <span className="badge badge-muted update-manager">{managerLabel(status.manager)}</span>}
          {status.scanned_at !== null
            ? <span title={`Scanned ${formatDateTime(status.scanned_at)} by the device’s clock`}>Scanned {formatRelative(status.received_at ?? status.scanned_at)}</span>
            : <span>Not scanned yet</span>}
          {status.pending > 0 && <><span aria-hidden="true">·</span><span>{status.pending === 1 ? "1 update pending" : `${status.pending} updates pending`}{status.security + status.critical > 0 ? `, ${status.security + status.critical} security or critical` : ""}</span></>}
        </p>
        <button
          type="button"
          className="secondary-button"
          onClick={() => void scan()}
          disabled={!agent.connected || scanning || !status.supported && status.scanned_at !== null}
          title={agent.connected ? "Ask the device to look for updates now" : "The device is offline"}
        >
          <RefreshCw size={15} aria-hidden="true" className={scanning ? "spin" : undefined} />
          {scanning ? "Scanning…" : "Scan now"}
        </button>
        {status.scanning && <p className="system-notice" role="status">Scanning since {formatRelative(status.scanning.created_at)}. Scans can take several minutes, longer on macOS and Windows.</p>}
        {notice && <p className="system-notice" role="alert">{notice}</p>}
      </div>
      {status.scan_error && (
        <div className="error-banner" role="alert">
          <SearchX size={16} aria-hidden="true" />
          <span>The last scan{status.received_at ? `, ${formatRelative(status.received_at)},` : ""} failed: {status.scan_error}{status.scanned_at !== null ? ` The list is from the scan before, ${formatRelative(status.scanned_at)}.` : ""}</span>
        </div>
      )}
      {status.reboot_pending && (
        <div className="notice-banner" role="status">
          <ShieldAlert size={16} aria-hidden="true" />
          <span>The device needs a restart to finish installing updates{status.reboot_pending_since ? `, pending for ${formatAge(status.reboot_pending_since)}` : ""}.</span>
        </div>
      )}
    </>
  );
}

// An install running on the device, and what it's doing.
function InstallProgress({ run }: { run: UpdateRun }) {
  return (
    <div className="install-progress" role="status">
      <div>
        <LoaderCircle size={16} className="spin" aria-hidden="true" />
        <strong>Installing updates</strong>
        <span>Started {formatRelative(run.created_at)} by {run.policy_name ? `policy “${run.policy_name}”` : run.requested_by}</span>
      </div>
      <progress max={100} value={run.progress_percent ?? undefined} aria-label="Install progress" />
      <span>{run.progress ?? "Waiting for the device to start…"}</span>
    </div>
  );
}
