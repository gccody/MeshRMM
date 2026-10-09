
import {
  CircleAlert,
  Download,
  Eye,
  File as FileIcon,
  FileCode,
  Folder,
  History,
  LoaderCircle,
  Lock,
  Pencil,
  Play,
  Plus,
  RefreshCw,
  Search,
  SquareTerminal,
  Trash2,
  Upload,
  Users,
  X,
} from "lucide-react";
import { type KeyboardEvent, type ReactNode, useCallback, useEffect, useMemo, useRef, useState } from "react";
import { AuthenticationRequired, type AuthorizedFetch, errorText } from "../../lib/http";
import type { Agent } from "../agents/types";
import { HeaderActions } from "../workspace/header-actions";
import { useWorkspace } from "../workspace/workspace-context";
import {
  LANGUAGE_LABELS,
  type ScriptRun,
  type Toolbox,
  type ToolboxFile,
  type ToolboxScript,
  folderSuggestions,
  formatBytes,
  groupByFolder,
  matchesQuery,
  ranAsLabel,
  runOutcome,
  runTone,
} from "./model";
import { toolboxAccess } from "./access";
import { type ToolboxDialog, ToolboxDialogs } from "./toolbox-dialogs";
import { deleteFile, deleteScript, downloadFile, fetchRuns, fetchToolbox } from "./toolbox-api";

type Tab = "scripts" | "files" | "runs";

const TABS: { id: Tab; label: string; icon: typeof FileCode }[] = [
  { id: "scripts", label: "Scripts", icon: FileCode },
  { id: "files", label: "Files", icon: FileIcon },
  { id: "runs", label: "Run history", icon: History },
];

type OpenDialog = (next: ToolboxDialog, event?: { currentTarget: HTMLElement }) => void;

// The toolbox's scripts and library files, and the runs of those scripts.
// Each item is private to the user who added it unless they share it.
export function ToolboxPanel() {
  const { authorizedFetch, inventory, can } = useWorkspace();
  const access = toolboxAccess(can);
  const tabs = TABS.filter(({ id }) => access.tabs.includes(id));
  const [tab, setTab] = useState<Tab>(access.tabs[0] ?? "scripts");
  const { toolbox, loadError, load, replaceScript, replaceFile } = useToolboxContents(authorizedFetch);
  const [actionError, setActionError] = useState<string | null>(null);
  const [query, setQuery] = useState("");
  const [dialog, setDialog] = useState<ToolboxDialog | null>(null);
  const [busyId, setBusyId] = useState<string | null>(null);
  const [runsVersion, setRunsVersion] = useState(0);
  const opener = useRef<HTMLElement | null>(null);

  const open: OpenDialog = (next, event) => {
    opener.current = event?.currentTarget ?? null;
    setActionError(null);
    setDialog(next);
  };
  const close = () => setDialog(null);

  const scripts = useMemo(() => toolbox?.scripts ?? [], [toolbox]);
  const files = useMemo(() => toolbox?.files ?? [], [toolbox]);
  const folders = useMemo(() => folderSuggestions([...scripts, ...files]), [scripts, files]);

  const removeScript = async (script: ToolboxScript) => {
    if (!window.confirm(`Delete the script “${script.name}”?${script.shared ? " Everyone in the company loses it." : ""} Past runs keep its name and output.`)) return;
    setBusyId(script.id);
    setActionError(null);
    try {
      await deleteScript(authorizedFetch, script.id);
      replaceScript(null, script.id);
    } catch (error) {
      if (!(error instanceof AuthenticationRequired)) setActionError(errorText(error, "The script could not be deleted."));
    } finally {
      setBusyId(null);
    }
  };

  const removeFile = async (file: ToolboxFile) => {
    if (!window.confirm(`Delete “${file.name}” from the toolbox?${file.shared ? " Everyone in the company loses it." : ""} Copies already sent to devices stay there.`)) return;
    setBusyId(file.id);
    setActionError(null);
    try {
      await deleteFile(authorizedFetch, file.id);
      replaceFile(null, file.id);
    } catch (error) {
      if (!(error instanceof AuthenticationRequired)) setActionError(errorText(error, "The file could not be deleted."));
    } finally {
      setBusyId(null);
    }
  };

  const download = async (file: ToolboxFile) => {
    setBusyId(file.id);
    setActionError(null);
    try {
      await downloadFile(authorizedFetch, file);
    } catch (error) {
      if (!(error instanceof AuthenticationRequired)) setActionError(errorText(error, `${file.name} could not be downloaded.`));
    } finally {
      setBusyId(null);
    }
  };

  const selectTab = (next: Tab) => {
    setTab(next);
    if (next === "runs") setRunsVersion((version) => version + 1);
  };

  const visibleScripts = scripts.filter((script) => matchesQuery(script, query));
  const visibleFiles = files.filter((file) => matchesQuery(file, query));

  return (
    <>
      {actionError && (
        <div className="error-banner" role="alert">
          <CircleAlert size={17} aria-hidden="true" /><span>{actionError}</span>
          <button onClick={() => setActionError(null)} aria-label="Dismiss"><X size={16} /></button>
        </div>
      )}
      <section className="agent-panel toolbox-panel">
        <HeaderActions>
          {tab === "scripts" && <>
            {access.runScripts && <button className="secondary-button" onClick={(event) => open({ kind: "run" }, event)} disabled={!scripts.length}><SquareTerminal size={16} /> Run a script</button>}
            {access.addScripts && <button className="primary-button" onClick={(event) => open({ kind: "script", script: null }, event)}><Plus size={16} /> New script</button>}
          </>}
          {tab === "files" && access.addFiles && <button className="primary-button" onClick={(event) => open({ kind: "upload" }, event)}><Upload size={16} /> Upload files</button>}
          {tab === "runs" && <button className="icon-button" onClick={() => setRunsVersion((version) => version + 1)} aria-label="Refresh" title="Refresh"><RefreshCw size={16} /></button>}
        </HeaderActions>
        <div className="panel-header">
          <ToolboxTabs tabs={tabs} selected={tab} onSelect={selectTab} counts={toolbox && { scripts: scripts.length, files: files.length }} />
          {tab !== "runs" && <label className="search-field"><Search size={16} aria-hidden="true" /><input type="search" aria-label={`Search ${tab}`} value={query} onChange={(event) => setQuery(event.target.value)} placeholder={`Search ${tab}`} /></label>}
        </div>

        <div id="toolbox-scripts" role="tabpanel" aria-labelledby="toolbox-tab-scripts" hidden={tab !== "scripts"}>
          <ToolboxList
            loading={!toolbox}
            error={loadError}
            onRetry={() => void load()}
            items={visibleScripts}
            filtered={Boolean(query.trim())}
            empty={{ title: "No scripts yet", detail: "Write a PowerShell or Command Prompt script to run on your devices." }}
            renderRow={(script) => <ScriptRow key={script.id} script={script} canRun={access.runScripts} busy={busyId === script.id} open={open} onDelete={() => void removeScript(script)} />}
          />
        </div>

        <div id="toolbox-files" role="tabpanel" aria-labelledby="toolbox-tab-files" hidden={tab !== "files"}>
          <ToolboxList
            loading={!toolbox}
            error={loadError}
            onRetry={() => void load()}
            items={visibleFiles}
            filtered={Boolean(query.trim())}
            empty={{ title: "No files yet", detail: "Upload installers and tools. In a remote session, the toolbox sends them to the device's Documents." }}
            renderRow={(file) => <FileRow key={file.id} file={file} busy={busyId === file.id} open={open} onDownload={() => void download(file)} onDelete={() => void removeFile(file)} />}
          />
        </div>

        <div id="toolbox-runs" role="tabpanel" aria-labelledby="toolbox-tab-runs" hidden={tab !== "runs"}>
          {tab === "runs" && <RunHistory version={runsVersion} agents={inventory.agents} onOpen={(run, event) => open({ kind: "run-details", run }, event)} />}
        </div>
      </section>

      <ToolboxDialogs
        dialog={dialog}
        access={access}
        scripts={scripts}
        folders={folders}
        maxBytes={toolbox?.max_file_bytes ?? null}
        agents={inventory.agents}
        returnFocus={opener}
        onClose={close}
        onRunClosed={() => { close(); setRunsVersion((version) => version + 1); }}
        replaceScript={replaceScript}
        replaceFile={replaceFile}
      />
    </>
  );
}

// The toolbox's scripts and files, kept in step with the changes made here.
function useToolboxContents(authorizedFetch: AuthorizedFetch) {
  const [toolbox, setToolbox] = useState<Toolbox | null>(null);
  const [loadError, setLoadError] = useState<string | null>(null);

  const load = useCallback(async () => {
    try {
      setToolbox(await fetchToolbox(authorizedFetch));
      setLoadError(null);
    } catch (error) {
      if (!(error instanceof AuthenticationRequired)) setLoadError(errorText(error, "The toolbox could not be loaded."));
    }
  }, [authorizedFetch]);

  useEffect(() => {
    let cancelled = false;
    fetchToolbox(authorizedFetch)
      .then((loaded) => { if (!cancelled) setToolbox(loaded); })
      .catch((error: unknown) => {
        if (!cancelled && !(error instanceof AuthenticationRequired)) setLoadError(errorText(error, "The toolbox could not be loaded."));
      });
    return () => { cancelled = true; };
  }, [authorizedFetch]);

  const replaceScript = (saved: ToolboxScript | null, id: string | null) => {
    setToolbox((current) => {
      if (!current) return current;
      const others = current.scripts.filter((script) => script.id !== (id ?? saved?.id));
      return { ...current, scripts: saved ? [...others, { ...saved, body: undefined }] : others };
    });
  };
  const replaceFile = (saved: ToolboxFile | null, id: string) => {
    setToolbox((current) => {
      if (!current) return current;
      const others = current.files.filter((file) => file.id !== id);
      return { ...current, files: saved ? [...others, saved] : others };
    });
  };

  return { toolbox, loadError, load, replaceScript, replaceFile };
}

function ToolboxTabs({ tabs, selected, onSelect, counts }: {
  tabs: typeof TABS;
  selected: Tab;
  onSelect: (tab: Tab) => void;
  // How many scripts and files there are, once the toolbox has loaded.
  counts: { scripts: number; files: number } | null;
}) {
  const onTabKey = (index: number) => (event: KeyboardEvent<HTMLButtonElement>) => {
    const step = event.key === "ArrowRight" ? 1 : event.key === "ArrowLeft" ? -1 : 0;
    if (!step) return;
    event.preventDefault();
    const next = tabs[(index + step + tabs.length) % tabs.length].id;
    onSelect(next);
    document.getElementById(`toolbox-tab-${next}`)?.focus();
  };
  return (
    <div className="toolbox-tabs" role="tablist" aria-label="Toolbox">
      {tabs.map(({ id, label, icon: Icon }, index) => (
        <button
          key={id}
          id={`toolbox-tab-${id}`}
          type="button"
          role="tab"
          aria-selected={selected === id}
          aria-controls={`toolbox-${id}`}
          tabIndex={selected === id ? 0 : -1}
          onClick={() => onSelect(id)}
          onKeyDown={onTabKey(index)}
        >
          <Icon size={16} aria-hidden="true" />{label}
          {id === "scripts" && counts && <em>{counts.scripts}</em>}
          {id === "files" && counts && <em>{counts.files}</em>}
        </button>
      ))}
    </div>
  );
}

function ScriptRow({ script, canRun, busy, open, onDelete }: { script: ToolboxScript; canRun: boolean; busy: boolean; open: OpenDialog; onDelete: () => void }) {
  return (
    <li className="toolbox-row">
      <FileCode size={18} className="toolbox-row-icon" aria-hidden="true" />
      <div className="toolbox-row-main">
        <strong>{script.name}</strong>
        <span>{LANGUAGE_LABELS[script.language]}{script.description ? ` · ${script.description}` : ""}</span>
      </div>
      <SharingBadge item={script} />
      <div className="row-actions">
        {canRun && <button className="remote-button" onClick={(event) => open({ kind: "run", scriptId: script.id }, event)}><Play size={15} /> Run</button>}
        <button className="close-session-button" onClick={(event) => open({ kind: "script", script }, event)} aria-label={`${script.can_edit ? "Edit" : "View"} ${script.name}`} title={script.can_edit ? "Edit" : "View"}>{script.can_edit ? <Pencil size={16} /> : <Eye size={16} />}</button>
        {script.can_edit && <button className="agent-delete-button" onClick={onDelete} disabled={busy} aria-label={`Delete ${script.name}`} title="Delete">{busy ? <LoaderCircle size={16} className="spin" /> : <Trash2 size={16} />}</button>}
      </div>
    </li>
  );
}

function FileRow({ file, busy, open, onDownload, onDelete }: { file: ToolboxFile; busy: boolean; open: OpenDialog; onDownload: () => void; onDelete: () => void }) {
  return (
    <li className="toolbox-row">
      <FileIcon size={18} className="toolbox-row-icon" aria-hidden="true" />
      <div className="toolbox-row-main">
        <strong>{file.name}</strong>
        <span>{formatBytes(file.size_bytes)}</span>
      </div>
      <SharingBadge item={file} />
      <div className="row-actions">
        <button className="close-session-button" onClick={onDownload} disabled={busy} aria-label={`Download ${file.name}`} title="Download">{busy ? <LoaderCircle size={16} className="spin" /> : <Download size={16} />}</button>
        <button className="close-session-button" onClick={(event) => open({ kind: "file", file }, event)} aria-label={`${file.can_edit ? "Edit" : "View"} ${file.name}`} title={file.can_edit ? "Edit" : "Details"}>{file.can_edit ? <Pencil size={16} /> : <Eye size={16} />}</button>
        {file.can_edit && <button className="agent-delete-button" onClick={onDelete} disabled={busy} aria-label={`Delete ${file.name}`} title="Delete"><Trash2 size={16} /></button>}
      </div>
    </li>
  );
}

function SharingBadge({ item }: { item: { shared: boolean; owned: boolean } }) {
  return item.shared
    ? <span className="toolbox-badge toolbox-badge-shared" title="Everyone with access to the toolbox can see and use it"><Users size={13} aria-hidden="true" />Shared</span>
    : <span className="toolbox-badge" title="Only you can see and use it"><Lock size={13} aria-hidden="true" />Private</span>;
}

function ToolboxList<T extends { id: string; folder: string; name: string }>({ loading, error, onRetry, items, filtered, empty, renderRow }: {
  loading: boolean;
  error: string | null;
  onRetry: () => void;
  items: T[];
  filtered: boolean;
  empty: { title: string; detail: string };
  renderRow: (item: T) => ReactNode;
}) {
  if (error && loading) {
    return <div className="empty-state"><CircleAlert size={28} /><strong>The toolbox could not be loaded</strong><span role="alert">{error}</span><button className="secondary-button" onClick={onRetry}><RefreshCw size={16} /> Try again</button></div>;
  }
  if (loading) return <div className="empty-state"><LoaderCircle size={28} className="spin" /><strong>Loading the toolbox…</strong></div>;
  if (!items.length) {
    return <div className="empty-state"><FileCode size={28} /><strong>{filtered ? "Nothing matches your search" : empty.title}</strong><span>{filtered ? "Try another name or folder." : empty.detail}</span></div>;
  }
  return (
    <div className="toolbox-groups">
      {groupByFolder(items).map(({ folder, items: grouped }) => (
        <section key={folder} className="toolbox-group" aria-label={folder || "Top level"}>
          <h3><Folder size={15} aria-hidden="true" />{folder ? folder.split("/").join(" / ") : "Top level"}</h3>
          <ul>{grouped.map(renderRow)}</ul>
        </section>
      ))}
    </div>
  );
}

const formatWhen = (unixMs: number) =>
  new Date(unixMs).toLocaleString([], { month: "short", day: "numeric", hour: "numeric", minute: "2-digit" });

function RunHistory({ version, agents, onOpen }: { version: number; agents: Agent[]; onOpen: (run: ScriptRun, event: { currentTarget: HTMLElement }) => void }) {
  const { authorizedFetch } = useWorkspace();
  const [runs, setRuns] = useState<ScriptRun[] | null>(null);
  const [error, setError] = useState<string | null>(null);
  const names = useMemo(() => new Map(agents.map((agent) => [agent.id, agent.name])), [agents]);

  useEffect(() => {
    let cancelled = false;
    fetchRuns(authorizedFetch)
      .then((loaded) => { if (!cancelled) { setRuns(loaded); setError(null); } })
      .catch((loadError: unknown) => {
        if (!cancelled && !(loadError instanceof AuthenticationRequired)) setError(errorText(loadError, "Recent runs could not be loaded."));
      });
    return () => { cancelled = true; };
  }, [authorizedFetch, version]);

  if (error && !runs) return <div className="empty-state"><CircleAlert size={28} /><strong>Recent runs could not be loaded</strong><span role="alert">{error}</span></div>;
  if (!runs) return <div className="empty-state"><LoaderCircle size={28} className="spin" /><strong>Loading recent runs…</strong></div>;
  if (!runs.length) return <div className="empty-state"><History size={28} /><strong>No runs yet</strong><span>Runs from the website and from remote sessions appear here for 30 days.</span></div>;
  return (
    <table className="run-table" aria-label="Recent script runs">
      <thead><tr><th scope="col">Script</th><th scope="col">Device</th><th scope="col">Run as</th><th scope="col">Result</th><th scope="col">Started</th><th scope="col"><span className="sr-only">Details</span></th></tr></thead>
      <tbody>
        {runs.map((run) => (
          <tr key={run.id}>
            <td><strong>{run.script_name}</strong>{!run.requested_by_you && <span>By a teammate</span>}</td>
            <td>{names.get(run.device_id) ?? run.device_id}</td>
            <td>{ranAsLabel(run)}</td>
            <td><span className={`run-pill run-pill-${runTone(run)}`}>{runOutcome(run)}</span></td>
            <td>{formatWhen(run.created_at_unix_ms)}</td>
            <td><button className="secondary-button" onClick={(event) => onOpen(run, event)}>View</button></td>
          </tr>
        ))}
      </tbody>
    </table>
  );
}
