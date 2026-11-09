import { ArrowUpCircle, CircleAlert, LoaderCircle, PackageCheck, PackagePlus, Pin, RefreshCw, Search, ServerOff, Trash2, X } from "lucide-react";
import { type FormEvent, type RefObject, useCallback, useEffect, useId, useMemo, useRef, useState } from "react";
import { formatDateTime, formatRelative } from "../../lib/format";
import { AuthenticationRequired, asSentence, errorText } from "../../lib/http";
import { ModalDialog } from "../../lib/modal-dialog";
import { useResource } from "../../lib/use-resource";
import { type Agent, PLATFORM_LABELS } from "../agents/types";
import { fetchApplications as fetchInventory } from "../system-info/system-info-api";
import { formatAge } from "../updates/model";
import { useWorkspace } from "../workspace/workspace-context";
import { fetchDevicePackages, fetchLibrary, requestPackageScan, startApplicationRun } from "./applications-api";
import {
  type ApplicationRun,
  type DevicePackage,
  type DevicePackages,
  type InventoryProgram,
  MAX_RUN_ITEMS,
  type RunRequest,
  SOURCE_LABELS,
  applicationsFor,
  filterPackages,
  packageKey,
  uninstallableInventory,
  versionChange,
} from "./model";
import { RunHistory } from "./run-history";

// While a scan or run is in progress, the packages are read this often.
const POLL_INTERVAL_MS = 10_000;

// `/device?id=…&tab=applications`: what the device's package managers have
// installed and outdated, with updates, uninstalls, installs from the
// library, and the device's history.
export function DeviceApplications({ agent }: { agent: Agent }) {
  const { authorizedFetch, can } = useWorkspace();
  const load = useCallback(() => fetchDevicePackages(authorizedFetch, agent.id), [authorizedFetch, agent.id]);
  const { data, error, reload, setData } = useResource(load, "The device’s applications could not be loaded.");
  const [historyKey, setHistoryKey] = useState(0);
  const busy = Boolean(data?.scanning || data?.running);
  const wasRunning = useRef(false);

  useEffect(() => {
    if (!busy) return;
    const timer = window.setInterval(reload, POLL_INTERVAL_MS);
    return () => window.clearInterval(timer);
  }, [busy, reload]);

  // A run that finishes shows in the history.
  const running = Boolean(data?.running);
  useEffect(() => {
    if (wasRunning.current && !running) setHistoryKey((key) => key + 1);
    wasRunning.current = running;
  }, [running]);

  const started = (run: ApplicationRun) => {
    setData((current) => (run.action === "scan" ? { ...current, scanning: run } : { ...current, running: run.status === "pending" ? run : null }));
    setHistoryKey((key) => key + 1);
    reload();
  };

  return (
    <section className="system-info updates-tab" aria-label="Applications">
      {!data ? (
        error
          ? <div className="empty-state compact"><CircleAlert size={22} aria-hidden="true" /><strong>{error}</strong><button type="button" className="secondary-button" onClick={reload}><RefreshCw size={16} /> Try again</button></div>
          : <div className="empty-state compact"><LoaderCircle size={22} className="spin" aria-hidden="true" /><strong>Loading applications…</strong></div>
      ) : (
        <>
          {error && <div className="error-banner" role="alert"><CircleAlert size={16} aria-hidden="true" /><span>{error}</span></div>}
          <PackagesBody agent={agent} data={data} canDeploy={can("applications.deploy")} onStarted={started} />
        </>
      )}
      {data && <RunHistory deviceId={agent.id} reloadKey={historyKey} />}
    </section>
  );
}

function PackagesBody({ agent, data, canDeploy, onStarted }: {
  agent: Agent;
  data: DevicePackages;
  canDeploy: boolean;
  onStarted: (run: ApplicationRun) => void;
}) {
  const { authorizedFetch } = useWorkspace();
  const [query, setQuery] = useState("");
  const [outdatedOnly, setOutdatedOnly] = useState(false);
  const [chosen, setChosen] = useState<ReadonlySet<string>>(() => new Set());
  const [dialog, setDialog] = useState<"install" | "inventory" | null>(null);
  const [starting, setStarting] = useState(false);
  const [notice, setNotice] = useState<string | null>(null);
  const opener = useRef<HTMLElement | null>(null);
  const shown = filterPackages(data.packages, query, outdatedOnly);
  const outdated = data.packages.filter((pkg) => pkg.available_version !== null && !pkg.held);
  const chosenPackages = data.packages.filter((pkg) => chosen.has(packageKey(pkg)));
  const chosenOutdated = chosenPackages.filter((pkg) => pkg.available_version !== null && !pkg.held);
  const blocked = !agent.connected ? "The device is offline." : data.running ? "Wait for the device to finish what it is doing." : null;
  const allShownChosen = shown.length > 0 && shown.every((pkg) => chosen.has(packageKey(pkg)));

  const toggle = (key: string, checked: boolean) => setChosen((current) => {
    const next = new Set(current);
    if (checked) next.add(key);
    else next.delete(key);
    return next;
  });
  const toggleShown = (checked: boolean) => setChosen((current) => {
    const next = new Set(current);
    for (const pkg of shown) {
      if (checked) next.add(packageKey(pkg));
      else next.delete(packageKey(pkg));
    }
    return next;
  });

  const run = async (request: RunRequest, failure: string) => {
    setStarting(true);
    setNotice(null);
    try {
      onStarted(await startApplicationRun(authorizedFetch, agent.id, request));
      setChosen(new Set());
    } catch (error) {
      if (!(error instanceof AuthenticationRequired)) setNotice(errorText(error, failure));
    } finally {
      setStarting(false);
    }
  };

  const update = (packages: readonly DevicePackage[] | null) => void run(
    packages ? { action: "update", mode: "only", packages: packages.map(packageKey) } : { action: "update", mode: "all" },
    "The update could not be started.",
  );
  const uninstall = (packages: readonly DevicePackage[]) => {
    const what = packages.length === 1 ? packages[0].name : `${packages.length} applications`;
    if (!window.confirm(`Uninstall ${what} from ${agent.name}?`)) return;
    void run({ action: "uninstall", packages: packages.map(packageKey) }, "The uninstall could not be started.");
  };
  const open = (which: "install" | "inventory", button: HTMLElement) => {
    opener.current = button;
    setDialog(which);
  };

  return (
    <>
      <ScanStatusBar agent={agent} data={data} onScanning={onStarted} />
      {data.running && <RunProgress run={data.running} />}
      {notice && <div className="error-banner" role="alert"><CircleAlert size={16} aria-hidden="true" /><span>{notice}</span><button onClick={() => setNotice(null)} aria-label="Dismiss"><X size={16} /></button></div>}
      {canDeploy && (
        <div className="selection-actions application-actions">
          <button type="button" className="secondary-button" disabled={blocked !== null || starting} title={blocked ?? undefined} onClick={(event) => open("install", event.currentTarget)} aria-haspopup="dialog"><PackagePlus size={15} /> Install from the library…</button>
          {agent.platform === "windows" && <button type="button" className="secondary-button" disabled={blocked !== null || starting} title={blocked ?? undefined} onClick={(event) => open("inventory", event.currentTarget)} aria-haspopup="dialog"><Trash2 size={15} /> Uninstall a program…</button>}
        </div>
      )}

      {data.status === null ? (
        <div className="empty-state compact">
          <ServerOff size={22} aria-hidden="true" />
          <strong>Not scanned yet</strong>
          <span>{agent.connected ? "The device lists its applications when it next looks for updates. Scan now asks it to look at once." : "The device lists its applications when it connects."}</span>
        </div>
      ) : data.status.managers.length === 0 ? (
        <div className="empty-state compact">
          <ServerOff size={22} aria-hidden="true" />
          <strong>No supported package manager</strong>
          <span>{noManagerHelp(agent)}</span>
        </div>
      ) : data.packages.length === 0 ? (
        <div className="empty-state compact">
          <PackageCheck size={22} aria-hidden="true" />
          <strong>No packages</strong>
          <span>The device’s package managers have nothing installed.</span>
        </div>
      ) : (
        <section className="agent-panel">
          <div className="table-toolbar">
            <label className="search-field software-search">
              <Search size={16} aria-hidden="true" />
              <input type="search" aria-label="Search applications" value={query} onChange={(event) => setQuery(event.target.value)} placeholder="Search applications" />
            </label>
            <div className="outcome-chips" role="group" aria-label="Show applications">
              <button type="button" className="outcome-chip" aria-pressed={!outdatedOnly} onClick={() => setOutdatedOnly(false)}>All<em>{data.packages.length}</em></button>
              <button type="button" className={`outcome-chip${outdated.length ? " outcome-problem" : ""}`} aria-pressed={outdatedOnly} onClick={() => setOutdatedOnly(true)}>Outdated<em>{outdated.length}</em></button>
            </div>
            {canDeploy && (
              <div className="selection-actions">
                {chosenPackages.length > 0 && <>
                  <button type="button" className="secondary-button" disabled={!chosenOutdated.length || blocked !== null || starting || chosenOutdated.length > MAX_RUN_ITEMS} title={blocked ?? undefined} onClick={() => update(chosenOutdated)}><ArrowUpCircle size={15} /> Update {chosenOutdated.length || ""} selected</button>
                  <button type="button" className="secondary-button" disabled={blocked !== null || starting || chosenPackages.length > MAX_RUN_ITEMS} title={blocked ?? undefined} onClick={() => uninstall(chosenPackages)}><Trash2 size={15} /> Uninstall {chosenPackages.length} selected</button>
                </>}
                <button type="button" className="primary-button" disabled={!outdated.length || blocked !== null || starting} title={blocked ?? undefined} onClick={() => update(null)}>{starting ? <LoaderCircle size={15} className="spin" /> : <ArrowUpCircle size={15} />} Update all</button>
              </div>
            )}
          </div>
          <div className="table-scroll software-scroll">
            <table className="data-table" aria-label="Installed applications">
              <thead>
                <tr>
                  {canDeploy && <th scope="col" className="select-cell"><input type="checkbox" aria-label="Select every application shown" checked={allShownChosen} onChange={(event) => toggleShown(event.target.checked)} /></th>}
                  <th scope="col">Application</th>
                  <th scope="col">Source</th>
                  <th scope="col">Version</th>
                  <th scope="col">Outdated for</th>
                  {canDeploy && <th scope="col"><span className="sr-only">Actions</span></th>}
                </tr>
              </thead>
              <tbody>
                {shown.map((pkg) => {
                  const key = packageKey(pkg);
                  const isOutdated = pkg.available_version !== null;
                  return (
                    <tr key={key} className={chosen.has(key) ? "selected-row" : undefined}>
                      {canDeploy && <td className="select-cell"><input type="checkbox" aria-label={`Select ${pkg.name}`} checked={chosen.has(key)} onChange={(event) => toggle(key, event.target.checked)} /></td>}
                      <td>
                        <strong>{pkg.name}</strong>
                        <span className="mono-detail">{pkg.package_id}</span>
                        {pkg.application_name && <span className="badge badge-muted">Library: {pkg.application_name}</span>}
                        {pkg.held && <span className="badge badge-muted" title="Its library application pins a version, so updates leave it alone"><Pin size={11} aria-hidden="true" />Pinned</span>}
                      </td>
                      <td>{SOURCE_LABELS[pkg.source]}</td>
                      <td>{isOutdated ? <strong className="outdated-version">{versionChange(pkg)}</strong> : pkg.version ?? "—"}</td>
                      <td className="numeric" title={pkg.outdated_since ? `Since ${formatDateTime(pkg.outdated_since)}` : undefined}>{pkg.outdated_since ? formatAge(pkg.outdated_since) : "—"}</td>
                      {canDeploy && (
                        <td className="row-actions">
                          {isOutdated && !pkg.held && <button type="button" className="link-button" disabled={blocked !== null || starting} onClick={() => update([pkg])}>Update</button>}
                          <button type="button" className="link-button" disabled={blocked !== null || starting} onClick={() => uninstall([pkg])}>Uninstall</button>
                        </td>
                      )}
                    </tr>
                  );
                })}
              </tbody>
            </table>
            {!shown.length && <p className="table-message field-help">{outdatedOnly && !query ? "Everything is up to date." : "No applications match."}</p>}
          </div>
        </section>
      )}

      {dialog === "install" && <LibraryInstallDialog agent={agent} returnFocus={opener} onClose={() => setDialog(null)} onStarted={(started) => { setDialog(null); onStarted(started); }} />}
      {dialog === "inventory" && <InventoryUninstallDialog agent={agent} returnFocus={opener} onClose={() => setDialog(null)} onStarted={(started) => { setDialog(null); onStarted(started); }} />}
    </>
  );
}

function noManagerHelp(agent: Agent) {
  if (agent.platform === "windows") return "MeshRMM uses winget and Chocolatey on Windows. Install either for the device to list and update applications.";
  if (agent.platform === "macos") return "MeshRMM uses Homebrew on macOS. Install it, owned by a regular user, for the device to list and update applications.";
  return "MeshRMM uses Flatpak (system installation) and Snap on Linux. Install either for the device to list and update applications.";
}

// The device's package managers, when it last scanned, and Scan now.
function ScanStatusBar({ agent, data, onScanning }: { agent: Agent; data: DevicePackages; onScanning: (run: ApplicationRun) => void }) {
  const { authorizedFetch } = useWorkspace();
  const [requesting, setRequesting] = useState(false);
  const [notice, setNotice] = useState<string | null>(null);
  const scanning = requesting || data.scanning !== null;
  const failed = data.status?.managers.filter((manager) => manager.error) ?? [];

  const scan = async () => {
    setNotice(null);
    setRequesting(true);
    try {
      onScanning(await requestPackageScan(authorizedFetch, agent.id));
    } catch (error) {
      if (!(error instanceof AuthenticationRequired)) setNotice(errorText(error, "The device could not be asked to scan for applications."));
    } finally {
      setRequesting(false);
    }
  };

  return (
    <>
      <div className="system-bar">
        <p className="status-line">
          {data.status?.managers.map((manager) => (
            <span key={manager.source} className="badge badge-muted update-manager" title={manager.error ?? undefined}>{SOURCE_LABELS[manager.source]}{manager.version ? ` ${manager.version}` : ""}</span>
          ))}
          {data.status
            ? <span title={`Scanned ${formatDateTime(data.status.scanned_at)} by the device’s clock`}>Scanned {formatRelative(data.status.received_at)}</span>
            : <span>Not scanned yet</span>}
        </p>
        <button type="button" className="secondary-button" onClick={() => void scan()} disabled={!agent.connected || scanning} title={agent.connected ? "Ask the device to list its applications now" : "The device is offline"}>
          <RefreshCw size={15} aria-hidden="true" className={scanning ? "spin" : undefined} />
          {scanning ? "Scanning…" : "Scan now"}
        </button>
        {data.scanning && <p className="system-notice" role="status">Scanning since {formatRelative(data.scanning.created_at)}. Package managers can take a few minutes to check for updates.</p>}
        {notice && <p className="system-notice" role="alert">{notice}</p>}
      </div>
      {failed.map((manager) => (
        <div key={manager.source} className="error-banner" role="alert">
          <CircleAlert size={16} aria-hidden="true" />
          <span>{SOURCE_LABELS[manager.source]} could not be read: {asSentence(manager.error ?? "")} Its packages are from the scan before.</span>
        </div>
      ))}
    </>
  );
}

// What the device is installing, updating or uninstalling.
function RunProgress({ run }: { run: ApplicationRun }) {
  return (
    <div className="install-progress" role="status">
      <div>
        <LoaderCircle size={16} className="spin" aria-hidden="true" />
        <strong>{run.summary}</strong>
        <span>Started {formatRelative(run.created_at)} by {run.policy_name ? `policy “${run.policy_name}”` : run.requested_by}</span>
      </div>
      <span>{run.progress ?? "Waiting for the device to start…"}</span>
    </div>
  );
}

// Installs chosen library applications on the device.
function LibraryInstallDialog({ agent, returnFocus, onClose, onStarted }: {
  agent: Agent;
  returnFocus: RefObject<HTMLElement | null>;
  onClose: () => void;
  onStarted: (run: ApplicationRun) => void;
}) {
  const { authorizedFetch } = useWorkspace();
  const titleId = useId();
  const load = useCallback(() => fetchLibrary(authorizedFetch), [authorizedFetch]);
  const { data, error: loadError } = useResource(load, "The application library could not be loaded.");
  const applications = useMemo(() => applicationsFor(data ?? [], agent.platform), [data, agent.platform]);
  const [chosen, setChosen] = useState<string[]>([]);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);

  const submit = async (event: FormEvent) => {
    event.preventDefault();
    if (!chosen.length) return;
    setBusy(true);
    setError(null);
    try {
      onStarted(await startApplicationRun(authorizedFetch, agent.id, { action: "install", application_ids: chosen }));
    } catch (failure) {
      if (!(failure instanceof AuthenticationRequired)) setError(errorText(failure, "The install could not be started."));
      setBusy(false);
    }
  };

  return (
    <ModalDialog className="settings-modal toolbox-modal" labelledBy={titleId} onClose={onClose} returnFocus={returnFocus}>
      <button type="button" className="modal-close" onClick={onClose} aria-label="Close"><X size={19} /></button>
      <h2 id={titleId}>Install on {agent.name}</h2>
      <p>The device installs each application from the first way the library lists for {PLATFORM_LABELS[agent.platform]} that works. Applications it already has are left as they are.</p>
      <form onSubmit={(event) => void submit(event)}>
        <fieldset className="choice-list picker-list" disabled={busy}>
          <legend>Applications</legend>
          {!data && !loadError && <p className="field-help"><LoaderCircle size={14} className="spin" aria-hidden="true" /> Loading the library…</p>}
          {data && !applications.length && <p className="field-help">The library has no applications for {PLATFORM_LABELS[agent.platform]} yet. Add them on the Applications page.</p>}
          {applications.map((application) => (
            <label key={application.id}>
              <input type="checkbox" checked={chosen.includes(application.id)} onChange={(event) => setChosen((current) => event.target.checked ? [...current, application.id] : current.filter((id) => id !== application.id))} />
              <span><strong>{application.name}</strong>{application.pinned_version ? `Version ${application.pinned_version}` : application.description}</span>
            </label>
          ))}
        </fieldset>
        {(error || loadError || chosen.length > MAX_RUN_ITEMS) && <p role="alert" className="installer-error">{error ?? loadError ?? `Choose at most ${MAX_RUN_ITEMS} applications.`}</p>}
        <div className="connection-reason-actions">
          <button type="button" className="secondary-button" onClick={onClose}>Cancel</button>
          <button className="primary-button" disabled={busy || !chosen.length || chosen.length > MAX_RUN_ITEMS}>{busy ? <LoaderCircle size={16} className="spin" /> : <PackagePlus size={16} />} Install</button>
        </div>
      </form>
    </ModalDialog>
  );
}

// Uninstalls programs from the device's Windows software list, whatever
// installed them.
function InventoryUninstallDialog({ agent, returnFocus, onClose, onStarted }: {
  agent: Agent;
  returnFocus: RefObject<HTMLElement | null>;
  onClose: () => void;
  onStarted: (run: ApplicationRun) => void;
}) {
  const { authorizedFetch } = useWorkspace();
  const titleId = useId();
  const load = useCallback(() => fetchInventory(authorizedFetch, agent.id), [authorizedFetch, agent.id]);
  const { data, error: loadError } = useResource(load, "The device’s software list could not be loaded.");
  const programs = useMemo(() => uninstallableInventory(data ?? []).sort((left, right) => left.name.localeCompare(right.name)), [data]);
  const [query, setQuery] = useState("");
  const [chosen, setChosen] = useState<InventoryProgram[]>([]);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const needle = query.trim().toLocaleLowerCase();
  const shown = programs.filter((program) => !needle || program.name.toLocaleLowerCase().includes(needle) || (program.publisher ?? "").toLocaleLowerCase().includes(needle));
  const isChosen = (program: { name: string; version: string | null }) => chosen.some((candidate) => candidate.name === program.name && (candidate.version ?? null) === program.version);

  const submit = async (event: FormEvent) => {
    event.preventDefault();
    if (!chosen.length) return;
    if (!window.confirm(`Uninstall ${chosen.length === 1 ? chosen[0].name : `${chosen.length} programs`} from ${agent.name}?`)) return;
    setBusy(true);
    setError(null);
    try {
      onStarted(await startApplicationRun(authorizedFetch, agent.id, { action: "uninstall", inventory: chosen }));
    } catch (failure) {
      if (!(failure instanceof AuthenticationRequired)) setError(errorText(failure, "The uninstall could not be started."));
      setBusy(false);
    }
  };

  return (
    <ModalDialog className="settings-modal toolbox-modal" labelledBy={titleId} onClose={onClose} returnFocus={returnFocus}>
      <button type="button" className="modal-close" onClick={onClose} aria-label="Close"><X size={19} /></button>
      <h2 id={titleId}>Uninstall programs from {agent.name}</h2>
      <p>Programs installed for every user, as Windows lists them. The device runs each program&apos;s quiet uninstaller, or removes it with Windows Installer. A program with neither can&apos;t be uninstalled this way; use a script, or add it to the library with an uninstall command.</p>
      <form onSubmit={(event) => void submit(event)}>
        <label className="search-field"><Search size={16} aria-hidden="true" /><input type="search" aria-label="Search programs" value={query} onChange={(event) => setQuery(event.target.value)} placeholder="Search programs" /></label>
        <fieldset className="choice-list picker-list" disabled={busy}>
          <legend className="sr-only">Programs</legend>
          {!data && !loadError && <p className="field-help"><LoaderCircle size={14} className="spin" aria-hidden="true" /> Loading the software list…</p>}
          {data && !shown.length && <p className="field-help">{programs.length ? "No programs match." : "The device hasn’t listed its software yet."}</p>}
          {shown.map((program) => (
            <label key={`${program.name}\u0000${program.version ?? ""}`}>
              <input type="checkbox" checked={isChosen(program)} onChange={(event) => setChosen((current) => event.target.checked
                ? [...current, { name: program.name, ...(program.version && { version: program.version }) }]
                : current.filter((candidate) => !(candidate.name === program.name && (candidate.version ?? null) === program.version)))} />
              <span><strong>{program.name}</strong>{[program.version, program.publisher].filter(Boolean).join(" · ")}</span>
            </label>
          ))}
        </fieldset>
        {(error || loadError || chosen.length > MAX_RUN_ITEMS) && <p role="alert" className="installer-error">{error ?? loadError ?? `Choose at most ${MAX_RUN_ITEMS} programs.`}</p>}
        <div className="connection-reason-actions">
          <button type="button" className="secondary-button" onClick={onClose}>Cancel</button>
          <button className="primary-button" disabled={busy || !chosen.length || chosen.length > MAX_RUN_ITEMS}>{busy ? <LoaderCircle size={16} className="spin" /> : <Trash2 size={16} />} Uninstall</button>
        </div>
      </form>
    </ModalDialog>
  );
}
