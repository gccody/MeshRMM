import { AppWindow, ArrowUpCircle, ChevronDown, ChevronRight, CircleAlert, Library, LoaderCircle, PackageCheck, Pencil, Pin, Plus, RefreshCw, Rocket, Search, Trash2, X } from "lucide-react";
import { Fragment, useCallback, useMemo, useRef, useState } from "react";
import { Link, useSearchParams } from "react-router";
import { formatDateTime, formatRelative } from "../../lib/format";
import { AuthenticationRequired, errorText } from "../../lib/http";
import { useResource } from "../../lib/use-resource";
import { type DevicePlatform, PLATFORM_LABELS } from "../agents/types";
import type { ToolboxFile } from "../toolbox/model";
import { fetchToolbox } from "../toolbox/toolbox-api";
import { HeaderActions } from "../workspace/header-actions";
import { useWorkspace } from "../workspace/workspace-context";
import { ApplicationEditor } from "./application-editor";
import { deleteApplication, fetchLibrary, fetchOutdated } from "./applications-api";
import { DeployDialog } from "./deploy-dialog";
import {
  type Application,
  MAX_RUN_ITEMS,
  type OutdatedPackage,
  type RunRequest,
  SOURCE_LABELS,
  SOURCE_PLATFORM,
  describeSource,
  namesText,
  oldestOutdated,
  outdatedTotals,
  packageKey,
  platformsOf,
} from "./model";

type Tab = "library" | "outdated";

type Deploy = { title: string; intro: string; request: RunRequest; platforms: DevicePlatform[]; deviceIds?: string[] };

// `/applications`: the library of applications to deploy, and every
// outdated package in the fleet.
export function ApplicationsPage() {
  const [searchParams] = useSearchParams();
  const tab: Tab = searchParams.get("tab") === "outdated" ? "outdated" : "library";
  return (
    <>
      <nav className="toolbox-tabs area-tabs" aria-label="Applications">
        <Link to="/applications" replace aria-current={tab === "library" ? "page" : undefined}><Library size={16} aria-hidden="true" />Library</Link>
        <Link to="/applications?tab=outdated" replace aria-current={tab === "outdated" ? "page" : undefined}><ArrowUpCircle size={16} aria-hidden="true" />Outdated</Link>
      </nav>
      {tab === "library" ? <LibraryTab /> : <OutdatedTab />}
    </>
  );
}

function LibraryTab() {
  const { authorizedFetch, can } = useWorkspace();
  const load = useCallback(() => fetchLibrary(authorizedFetch), [authorizedFetch]);
  const { data, error, reload, setData } = useResource(load, "The application library could not be loaded.");
  const [query, setQuery] = useState("");
  const [editing, setEditing] = useState<{ application: Application | null } | null>(null);
  const [files, setFiles] = useState<ToolboxFile[] | null>(null);
  const [deploy, setDeploy] = useState<Deploy | null>(null);
  const [actionError, setActionError] = useState<string | null>(null);
  const opener = useRef<HTMLElement | null>(null);
  const canManage = can("applications.manage");
  const canDeploy = can("applications.deploy");
  const needle = query.trim().toLocaleLowerCase();
  const shown = (data ?? []).filter((application) => !needle
    || application.name.toLocaleLowerCase().includes(needle)
    || application.sources.some((source) => source.package_id.toLocaleLowerCase().includes(needle)));

  const edit = (application: Application | null, button: HTMLElement) => {
    opener.current = button;
    setEditing({ application });
    if (files === null) {
      fetchToolbox(authorizedFetch).then(
        (toolbox) => setFiles(toolbox.files.filter((file) => file.shared)),
        () => setFiles([]),
      );
    }
  };
  const remove = async (application: Application) => {
    if (!window.confirm(`Remove ${application.name} from the library? Devices keep it installed.`)) return;
    setActionError(null);
    try {
      await deleteApplication(authorizedFetch, application.id);
      setData((current) => current.filter((candidate) => candidate.id !== application.id));
    } catch (failure) {
      if (!(failure instanceof AuthenticationRequired)) setActionError(errorText(failure, "The application could not be deleted."));
    }
  };
  const deployOne = (application: Application, button: HTMLElement) => {
    opener.current = button;
    setDeploy({
      title: `Install ${application.name}`,
      intro: "Each device installs it from the first way the library lists for its operating system that works; devices that have it already are left as they are.",
      request: { action: "install", application_ids: [application.id] },
      platforms: platformsOf(application),
    });
  };

  return (
    <>
      <HeaderActions>
        <button type="button" className="icon-button" onClick={reload} aria-label="Refresh" title="Refresh"><RefreshCw size={16} /></button>
        {canManage && <button type="button" className="primary-button" onClick={(event) => edit(null, event.currentTarget)} aria-haspopup="dialog"><Plus size={16} /> Add application</button>}
      </HeaderActions>
      {actionError && (
        <div className="error-banner" role="alert">
          <CircleAlert size={17} aria-hidden="true" /><span>{actionError}</span>
          <button onClick={() => setActionError(null)} aria-label="Dismiss"><X size={16} /></button>
        </div>
      )}
      <section className="agent-panel">
        {!data ? (
          error
            ? <div className="empty-state"><CircleAlert size={22} aria-hidden="true" /><strong>{error}</strong><button type="button" className="secondary-button" onClick={reload}><RefreshCw size={16} /> Try again</button></div>
            : <div className="empty-state"><LoaderCircle size={22} className="spin" aria-hidden="true" /><strong>Loading the library…</strong></div>
        ) : data.length === 0 ? (
          <div className="empty-state">
            <AppWindow size={22} aria-hidden="true" />
            <strong>No applications yet</strong>
            <span>Add the applications you deploy: a winget or Chocolatey package for Windows, a Homebrew formula or cask for macOS, a Flatpak or Snap for Linux, or an installer from the toolbox.{canManage ? "" : " Adding them needs the applications.manage permission."}</span>
          </div>
        ) : (
          <>
            <div className="table-toolbar">
              <label className="search-field software-search">
                <Search size={16} aria-hidden="true" />
                <input type="search" aria-label="Search the library" value={query} onChange={(event) => setQuery(event.target.value)} placeholder="Search the library" />
              </label>
            </div>
            <div className="table-scroll">
              <table className="data-table" aria-label="Application library">
                <thead>
                  <tr>
                    <th scope="col">Application</th>
                    <th scope="col">Installs with</th>
                    <th scope="col" className="numeric">Installed on</th>
                    <th scope="col" className="numeric">Outdated on</th>
                    <th scope="col"><span className="sr-only">Actions</span></th>
                  </tr>
                </thead>
                <tbody>
                  {shown.map((application) => (
                    <tr key={application.id}>
                      <td>
                        <strong>{application.name}</strong>
                        {application.description && <span>{application.description}</span>}
                        {application.pinned_version && <span className="badge badge-muted"><Pin size={11} aria-hidden="true" />Version {application.pinned_version}</span>}
                        {application.policies.length > 0 && <span>Policies: {application.policies.join(", ")}</span>}
                      </td>
                      <td>
                        {application.sources.map((source, index) => (
                          <span key={index} className={source.source === "custom" && !source.file_name ? "error-text" : undefined}>{PLATFORM_LABELS[source.platform]}: {describeSource(source)}</span>
                        ))}
                      </td>
                      <td className="numeric">{application.installed}</td>
                      <td className="numeric">{application.outdated ? <Link to="/applications?tab=outdated">{application.outdated}</Link> : 0}</td>
                      <td className="row-actions">
                        {canDeploy && <button type="button" className="close-session-button" onClick={(event) => deployOne(application, event.currentTarget)} aria-haspopup="dialog" aria-label={`Install ${application.name} on devices`} title="Install on devices"><Rocket size={16} /></button>}
                        {canManage && <>
                          <button type="button" className="close-session-button" onClick={(event) => edit(application, event.currentTarget)} aria-haspopup="dialog" aria-label={`Edit ${application.name}`} title="Edit"><Pencil size={16} /></button>
                          <button type="button" className="agent-delete-button" onClick={() => void remove(application)} aria-label={`Delete ${application.name}`} title="Delete"><Trash2 size={16} /></button>
                        </>}
                      </td>
                    </tr>
                  ))}
                </tbody>
              </table>
              {!shown.length && <p className="table-message field-help">No applications match.</p>}
            </div>
          </>
        )}
      </section>
      {editing && (
        <ApplicationEditor
          application={editing.application}
          files={files}
          returnFocus={opener}
          onClose={() => setEditing(null)}
          onSaved={(saved) => {
            setEditing(null);
            setData((current) => {
              const others = current.filter((candidate) => candidate.id !== saved.id);
              return [...others, saved].sort((left, right) => left.name.localeCompare(right.name));
            });
          }}
        />
      )}
      {deploy && <DeployDialog {...deploy} returnFocus={opener} onClose={() => setDeploy(null)} />}
    </>
  );
}

function OutdatedTab() {
  const { authorizedFetch, can } = useWorkspace();
  const load = useCallback(() => fetchOutdated(authorizedFetch), [authorizedFetch]);
  const { data, error, reload } = useResource(load, "The outdated applications could not be loaded.");
  const [query, setQuery] = useState("");
  const [chosen, setChosen] = useState<ReadonlySet<string>>(() => new Set());
  const [open, setOpen] = useState<string | null>(null);
  const [deploy, setDeploy] = useState<Deploy | null>(null);
  const opener = useRef<HTMLElement | null>(null);
  const canDeploy = can("applications.deploy");
  const needle = query.trim().toLocaleLowerCase();
  const packages = useMemo(() => (data ?? []).filter((pkg) => !needle
    || [pkg.name, pkg.package_id, pkg.application_name ?? ""].some((text) => text.toLocaleLowerCase().includes(needle))), [data, needle]);
  const totals = outdatedTotals(data ?? []);
  const chosenPackages = (data ?? []).filter((pkg) => chosen.has(packageKey(pkg)));
  const allShownChosen = packages.length > 0 && packages.every((pkg) => chosen.has(packageKey(pkg)));

  const toggle = (key: string, checked: boolean) => setChosen((current) => {
    const next = new Set(current);
    if (checked) next.add(key);
    else next.delete(key);
    return next;
  });
  const toggleShown = (checked: boolean) => setChosen((current) => {
    const next = new Set(current);
    for (const pkg of packages) {
      if (checked) next.add(packageKey(pkg));
      else next.delete(packageKey(pkg));
    }
    return next;
  });
  const devicesOf = (list: readonly OutdatedPackage[]) => [...new Set(list.flatMap((pkg) => pkg.devices.map((device) => device.device_id)))];
  const platformsOfPackages = (list: readonly OutdatedPackage[]) =>
    [...new Set(list.map((pkg) => (pkg.source === "custom" ? null : SOURCE_PLATFORM[pkg.source])).filter((platform): platform is DevicePlatform => platform !== null))];

  const updateChosen = (button: HTMLElement) => {
    opener.current = button;
    const names = chosenPackages.map((pkg) => pkg.application_name ?? pkg.name);
    setDeploy({
      title: `Update ${namesText(names)}`,
      intro: "Each device updates the chosen applications it has outdated, then lists its applications again.",
      request: { action: "update", mode: "only", packages: chosenPackages.map(packageKey) },
      platforms: platformsOfPackages(chosenPackages),
      deviceIds: devicesOf(chosenPackages),
    });
  };
  const updateAll = (button: HTMLElement) => {
    opener.current = button;
    setDeploy({
      title: "Update every outdated application",
      intro: "Each device updates every outdated application its package managers list, except those the library pins to a version.",
      request: { action: "update", mode: "all" },
      platforms: ["windows", "macos", "linux"],
      deviceIds: devicesOf((data ?? []).filter((pkg) => !pkg.held)),
    });
  };

  return (
    <>
      <HeaderActions>
        <button type="button" className="icon-button" onClick={reload} aria-label="Refresh" title="Refresh"><RefreshCw size={16} /></button>
        {canDeploy && data && data.length > 0 && <button type="button" className="primary-button" onClick={(event) => updateAll(event.currentTarget)} aria-haspopup="dialog"><ArrowUpCircle size={16} /> Update all…</button>}
      </HeaderActions>
      {data && (
        <div className="update-tiles" role="group" aria-label="Outdated applications">
          <div className={`update-tile${totals.packages ? " attention" : ""}`}><span>Outdated applications</span><strong>{totals.packages}</strong><small>{totals.packages ? "across the fleet" : "Everything is up to date"}</small></div>
          <div className="update-tile"><span>Devices with updates</span><strong>{totals.devices}</strong><small>{totals.updates} {totals.updates === 1 ? "update" : "updates"} pending</small></div>
        </div>
      )}
      {canDeploy && chosenPackages.length > 0 && (
        <div className="selection-bar" role="region" aria-label="Chosen applications">
          <strong>{chosenPackages.length} chosen</strong>
          <div className="selection-actions">
            <button type="button" className="primary-button" disabled={chosenPackages.length > MAX_RUN_ITEMS} title={chosenPackages.length > MAX_RUN_ITEMS ? `Choose at most ${MAX_RUN_ITEMS}` : undefined} onClick={(event) => updateChosen(event.currentTarget)} aria-haspopup="dialog"><ArrowUpCircle size={15} /> Update on their devices…</button>
            <button type="button" className="secondary-button" onClick={() => setChosen(new Set())}>Clear</button>
          </div>
        </div>
      )}
      <section className="agent-panel">
        {!data ? (
          error
            ? <div className="empty-state"><CircleAlert size={22} aria-hidden="true" /><strong>{error}</strong><button type="button" className="secondary-button" onClick={reload}><RefreshCw size={16} /> Try again</button></div>
            : <div className="empty-state"><LoaderCircle size={22} className="spin" aria-hidden="true" /><strong>Loading outdated applications…</strong></div>
        ) : data.length === 0 ? (
          <div className="empty-state">
            <PackageCheck size={22} aria-hidden="true" />
            <strong>Everything is up to date</strong>
            <span>Devices list their applications when they look for operating system updates, and outdated ones show here.</span>
          </div>
        ) : (
          <>
            <div className="table-toolbar">
              <label className="search-field software-search">
                <Search size={16} aria-hidden="true" />
                <input type="search" aria-label="Search applications" value={query} onChange={(event) => setQuery(event.target.value)} placeholder="Search applications" />
              </label>
            </div>
            <div className="table-scroll">
              <table className="data-table" aria-label="Outdated applications">
                <thead>
                  <tr>
                    {canDeploy && <th scope="col" className="select-cell"><input type="checkbox" aria-label={`Choose all ${packages.length} applications shown`} checked={allShownChosen} onChange={(event) => toggleShown(event.target.checked)} /></th>}
                    <th scope="col">Application</th>
                    <th scope="col">Source</th>
                    <th scope="col" className="numeric">Devices</th>
                    <th scope="col">Oldest</th>
                  </tr>
                </thead>
                <tbody>
                  {packages.map((pkg) => {
                    const key = packageKey(pkg);
                    const oldest = oldestOutdated(pkg);
                    return (
                      <Fragment key={key}>
                        <tr className={chosen.has(key) ? "selected-row" : undefined}>
                          {canDeploy && <td className="select-cell"><input type="checkbox" aria-label={`Choose ${pkg.name}`} checked={chosen.has(key)} disabled={pkg.held} onChange={(event) => toggle(key, event.target.checked)} /></td>}
                          <td>
                            <button type="button" className="link-button" aria-expanded={open === key} onClick={() => setOpen(open === key ? null : key)}>
                              {open === key ? <ChevronDown size={14} aria-hidden="true" /> : <ChevronRight size={14} aria-hidden="true" />}<strong>{pkg.name}</strong>
                            </button>
                            {pkg.application_name && <span className="badge badge-muted">Library: {pkg.application_name}</span>}
                            {pkg.held && <span className="badge badge-muted" title="Its library application pins a version, so updates leave it alone"><Pin size={11} aria-hidden="true" />Pinned</span>}
                          </td>
                          <td>{SOURCE_LABELS[pkg.source]}<span className="mono-detail">{pkg.package_id}</span></td>
                          <td className="numeric">{pkg.devices.length}</td>
                          <td title={oldest ? formatDateTime(oldest) : undefined}>{oldest ? formatRelative(oldest) : "—"}</td>
                        </tr>
                        {open === key && (
                          <tr>
                            <td colSpan={canDeploy ? 5 : 4}>
                              <ul className="run-items">
                                {pkg.devices.map((device) => (
                                  <li key={device.device_id}>
                                    <Link to={`/device?${new URLSearchParams({ id: device.device_id, tab: "applications" })}`}>{device.device_name}</Link>
                                    <span>{device.version ?? "?"} → {device.available_version ?? "newer"}</span>
                                    {device.outdated_since && <span>since {formatRelative(device.outdated_since)}</span>}
                                  </li>
                                ))}
                              </ul>
                            </td>
                          </tr>
                        )}
                      </Fragment>
                    );
                  })}
                </tbody>
              </table>
              {!packages.length && <p className="table-message field-help">No applications match.</p>}
            </div>
          </>
        )}
      </section>
      {deploy && <DeployDialog {...deploy} returnFocus={opener} onClose={() => setDeploy(null)} />}
    </>
  );
}
