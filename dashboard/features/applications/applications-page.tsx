import { AppWindow, ArrowUpCircle, ChevronDown, ChevronRight, CircleAlert, Library, LoaderCircle, PackageCheck, Pencil, Pin, Plus, RefreshCw, Rocket, Search, Trash2, X } from "lucide-react";
import { Fragment, useCallback, useRef, useState } from "react";
import { Link, useSearchParams } from "react-router";
import { formatDateTime, formatRelative } from "../../lib/format";
import { AuthenticationRequired, errorText } from "../../lib/http";
import { ShowMore } from "../../lib/show-more";
import { usePagedList } from "../../lib/use-paged-list";
import { useResource } from "../../lib/use-resource";
import { useSettled } from "../../lib/use-settled";
import { type DevicePlatform, PLATFORM_LABELS } from "../agents/types";
import type { ToolboxFile } from "../toolbox/model";
import { fetchToolbox } from "../toolbox/toolbox-api";
import { HeaderActions } from "../workspace/header-actions";
import { useWorkspace } from "../workspace/workspace-context";
import { ApplicationEditor } from "./application-editor";
import { deleteApplication, fetchLibrary, fetchOutdated, fetchOutdatedDevices } from "./applications-api";
import { DeployDialog, type OutdatedChoice } from "./deploy-dialog";
import {
  type Application,
  MAX_OUTDATED_QUERY_LENGTH,
  MAX_RUN_ITEMS,
  OUTDATED_PAGE_SIZE,
  type OutdatedPackage,
  type OutdatedTotals,
  type RunRequest,
  SOURCE_LABELS,
  SOURCE_PLATFORM,
  chosenUpdates,
  describeSource,
  joinDevicePage,
  namesText,
  outdatedLabel,
  outdatedSummary,
  packageKey,
  platformsOf,
} from "./model";

type Tab = "library" | "outdated";

type Deploy = { title: string; intro: string; request: RunRequest; platforms: DevicePlatform[]; deviceIds?: string[]; outdated?: OutdatedChoice };

// `/applications`: the library of applications to deploy, and the fleet's
// outdated packages, which the server counts and sends a page at a time.
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

const OUTDATED_MESSAGES = { first: "The outdated applications could not be loaded.", more: "More outdated applications could not be loaded." };

function OutdatedTab() {
  const { authorizedFetch, can } = useWorkspace();
  const [query, setQuery] = useState("");
  // Typing reaches the server after a pause.
  const settled = useSettled(query);
  const [totals, setTotals] = useState<OutdatedTotals | null>(null);
  const fetchPage = useCallback(async (offset: number) => {
    const page = await fetchOutdated(authorizedFetch, settled, offset);
    setTotals(page.totals);
    return { items: page.packages, matching: page.matching };
  }, [authorizedFetch, settled]);
  const { list, error, reload, showMore, loadingMore, remaining, current } = usePagedList(settled.trim(), fetchPage, packageKey, OUTDATED_MESSAGES);
  // Kept across searches, by key.
  const [chosen, setChosen] = useState<ReadonlyMap<string, OutdatedPackage>>(() => new Map());
  const [open, setOpen] = useState<string | null>(null);
  const [deploy, setDeploy] = useState<Deploy | null>(null);
  const opener = useRef<HTMLElement | null>(null);
  const canDeploy = can("applications.deploy");
  const packages = list?.items ?? [];
  const choosable = packages.filter((pkg) => !pkg.held);
  const chosenPackages = [...chosen.values()];
  const allShownChosen = choosable.length > 0 && choosable.every((pkg) => chosen.has(packageKey(pkg)));
  const searching = Boolean(settled.trim());

  const toggle = (pkg: OutdatedPackage, checked: boolean) => setChosen((current) => {
    const next = new Map(current);
    if (checked) next.set(packageKey(pkg), pkg);
    else next.delete(packageKey(pkg));
    return next;
  });
  const toggleShown = (checked: boolean) => setChosen((current) => {
    const next = new Map(current);
    for (const pkg of choosable) {
      if (checked) next.set(packageKey(pkg), pkg);
      else next.delete(packageKey(pkg));
    }
    return next;
  });
  const platformsOfPackages = (list: readonly OutdatedPackage[]) =>
    [...new Set(list.map((pkg) => (pkg.source === "custom" ? null : SOURCE_PLATFORM[pkg.source])).filter((platform): platform is DevicePlatform => platform !== null))];

  const updateChosen = (button: HTMLElement) => {
    opener.current = button;
    const names = chosenPackages.map((pkg) => pkg.application_name ?? pkg.name);
    const keys = chosenPackages.map(packageKey);
    setDeploy({
      title: `Update ${namesText(names)}`,
      intro: "Each device updates the chosen applications it has outdated, then lists its applications again.",
      request: { action: "update", mode: "only", packages: keys },
      platforms: platformsOfPackages(chosenPackages),
      outdated: { packages: keys, label: outdatedLabel(names), updates: chosenUpdates(chosenPackages) },
    });
  };
  const updateAll = (button: HTMLElement) => {
    opener.current = button;
    setDeploy({
      title: "Update every outdated application",
      intro: "Each device updates every outdated application its package managers list, except those the library pins to a version.",
      request: { action: "update", mode: "all" },
      platforms: ["windows", "macos", "linux"],
      outdated: { packages: [], label: outdatedLabel([]), updates: totals?.updates ?? 0 },
    });
  };

  return (
    <>
      <HeaderActions>
        <button type="button" className="icon-button" onClick={reload} aria-label="Refresh" title="Refresh"><RefreshCw size={16} /></button>
        {canDeploy && totals && totals.packages > 0 && <button type="button" className="primary-button" onClick={(event) => updateAll(event.currentTarget)} aria-haspopup="dialog"><ArrowUpCircle size={16} /> Update all…</button>}
      </HeaderActions>
      {totals && (
        <div className="update-tiles" role="group" aria-label="Outdated applications">
          <div className={`update-tile${totals.packages ? " attention" : ""}`}><span>Outdated applications</span><strong>{totals.packages}</strong><small>{totals.packages ? "across the fleet" : "Everything is up to date"}</small></div>
          <div className="update-tile"><span>Updates pending</span><strong>{totals.updates}</strong><small>{totals.updates === 1 ? "an application on a device" : "applications on devices"}</small></div>
        </div>
      )}
      {canDeploy && chosenPackages.length > 0 && (
        <div className="selection-bar" role="region" aria-label="Chosen applications">
          <strong>{chosenPackages.length} chosen</strong>
          <div className="selection-actions">
            <button type="button" className="primary-button" disabled={chosenPackages.length > MAX_RUN_ITEMS} title={chosenPackages.length > MAX_RUN_ITEMS ? `Choose at most ${MAX_RUN_ITEMS}` : undefined} onClick={(event) => updateChosen(event.currentTarget)} aria-haspopup="dialog"><ArrowUpCircle size={15} /> Update on their devices…</button>
            <button type="button" className="secondary-button" onClick={() => setChosen(new Map())}>Clear</button>
          </div>
        </div>
      )}
      <section className="agent-panel">
        <div className="table-toolbar">
          <label className="search-field software-search">
            <Search size={16} aria-hidden="true" />
            <input type="search" aria-label="Search applications" value={query} maxLength={MAX_OUTDATED_QUERY_LENGTH} onChange={(event) => setQuery(event.target.value)} placeholder="Search applications" />
          </label>
        </div>
        {!list ? (
          error
            ? <div className="empty-state"><CircleAlert size={22} aria-hidden="true" /><strong>{error}</strong><button type="button" className="secondary-button" onClick={reload}><RefreshCw size={16} /> Try again</button></div>
            : <div className="empty-state"><LoaderCircle size={22} className="spin" aria-hidden="true" /><strong>Loading outdated applications…</strong></div>
        ) : packages.length === 0 && remaining === 0 ? (
          searching
            ? <p className="table-message field-help">No outdated applications match.</p>
            : (
              <div className="empty-state">
                <PackageCheck size={22} aria-hidden="true" />
                <strong>Everything is up to date</strong>
                <span>Devices list their applications when they look for operating system updates, and outdated ones show here.</span>
              </div>
            )
        ) : (
          <>
            {error && <p className="table-message form-error" role="alert"><CircleAlert size={16} aria-hidden="true" />{error}</p>}
            <div className="table-scroll">
              <table className="data-table" aria-label="Outdated applications" aria-busy={!current}>
                <thead>
                  <tr>
                    {canDeploy && <th scope="col" className="select-cell"><input type="checkbox" aria-label={`Choose all ${choosable.length} applications shown`} checked={allShownChosen} disabled={!choosable.length} onChange={(event) => toggleShown(event.target.checked)} /></th>}
                    <th scope="col">Application</th>
                    <th scope="col">Source</th>
                    <th scope="col">Newest version</th>
                    <th scope="col" className="numeric">Devices</th>
                  </tr>
                </thead>
                <tbody>
                  {packages.map((pkg) => {
                    const key = packageKey(pkg);
                    return (
                      <Fragment key={key}>
                        <tr className={chosen.has(key) ? "selected-row" : undefined}>
                          {canDeploy && <td className="select-cell"><input type="checkbox" aria-label={`Choose ${pkg.name}`} checked={chosen.has(key)} disabled={pkg.held} onChange={(event) => toggle(pkg, event.target.checked)} /></td>}
                          <td>
                            <button type="button" className="link-button" aria-expanded={open === key} onClick={() => setOpen(open === key ? null : key)}>
                              {open === key ? <ChevronDown size={14} aria-hidden="true" /> : <ChevronRight size={14} aria-hidden="true" />}<strong>{pkg.name}</strong>
                            </button>
                            {pkg.application_name && <span className="badge badge-muted">Library: {pkg.application_name}</span>}
                            {pkg.held && <span className="badge badge-muted" title="Its library application pins a version, so updates leave it alone"><Pin size={11} aria-hidden="true" />Pinned</span>}
                          </td>
                          <td>{SOURCE_LABELS[pkg.source]}<span className="mono-detail">{pkg.package_id}</span></td>
                          <td>{pkg.available_version ?? "—"}</td>
                          <td className="numeric">{pkg.devices}</td>
                        </tr>
                        {open === key && (
                          <tr>
                            <td colSpan={canDeploy ? 5 : 4}><OutdatedDevices pkg={pkg} /></td>
                          </tr>
                        )}
                      </Fragment>
                    );
                  })}
                </tbody>
              </table>
            </div>
            <div className="panel-footer fleet-footer">
              <span className="field-help">
                {outdatedSummary(list.matching, searching)}
                {remaining > 0 && ` · ${packages.length} loaded`}
              </span>
              <ShowMore remaining={remaining} pageSize={OUTDATED_PAGE_SIZE} loading={loadingMore} disabled={!current} onShowMore={() => void showMore()} />
            </div>
          </>
        )}
      </section>
      {deploy && <DeployDialog {...deploy} returnFocus={opener} onClose={() => setDeploy(null)} />}
    </>
  );
}

// The devices with an opened package outdated, a page at a time.
function OutdatedDevices({ pkg }: { pkg: OutdatedPackage }) {
  const { authorizedFetch } = useWorkspace();
  const { source, package_id } = pkg;
  const load = useCallback(() => fetchOutdatedDevices(authorizedFetch, { source, package_id }, null), [authorizedFetch, source, package_id]);
  const { data, error, reload, setData } = useResource(load, "The devices could not be loaded.");
  const [moreError, setMoreError] = useState<string | null>(null);
  const [loading, setLoading] = useState(false);

  const showMore = async (after: string) => {
    setLoading(true);
    setMoreError(null);
    try {
      const page = await fetchOutdatedDevices(authorizedFetch, { source, package_id }, after);
      setData((current) => joinDevicePage(current, after, page));
    } catch (failure) {
      if (!(failure instanceof AuthenticationRequired)) setMoreError(errorText(failure, "More devices could not be loaded."));
    }
    setLoading(false);
  };

  if (!data) {
    return error
      ? <p className="form-error" role="alert"><CircleAlert size={16} aria-hidden="true" />{error} <button type="button" className="link-button" onClick={reload}>Try again</button></p>
      : <p className="field-help"><LoaderCircle size={15} className="spin" aria-hidden="true" /> Loading its devices…</p>;
  }
  const { next } = data;
  return (
    <>
      {data.devices.length === 0 && <p className="field-help">No device has it outdated any more.</p>}
      <ul className="run-items">
        {data.devices.map((device) => (
          <li key={device.device_id}>
            <Link to={`/device?${new URLSearchParams({ id: device.device_id, tab: "applications" })}`}>{device.device_name}</Link>
            <span>{device.version ?? "?"} → {device.available_version ?? "newer"}</span>
            {device.outdated_since && <span title={formatDateTime(device.outdated_since)}>since {formatRelative(device.outdated_since)}</span>}
          </li>
        ))}
      </ul>
      {moreError && <p className="form-error" role="alert"><CircleAlert size={16} aria-hidden="true" />{moreError}</p>}
      {next && (
        <button type="button" className="secondary-button" disabled={loading} onClick={() => void showMore(next)}>
          {loading && <LoaderCircle size={15} className="spin" />} Show more devices
        </button>
      )}
    </>
  );
}
