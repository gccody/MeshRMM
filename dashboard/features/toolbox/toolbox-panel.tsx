
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
import { AuthenticationRequired, errorText } from "../../lib/http";
import { ModalDialog } from "../../lib/modal-dialog";
import type { Agent } from "../agents/types";
import { useWorkspace } from "../workspace/workspace-context";
import { FileDetailsModal, UploadFilesModal } from "./file-dialogs";
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
import { RunResult, useScriptRun } from "./run-result";
import { RunScriptModal } from "./run-script-modal";
import { ScriptEditor } from "./script-editor";
import { toolboxAccess } from "./access";
import { deleteFile, deleteScript, downloadFile, fetchRuns, fetchToolbox } from "./toolbox-api";

type Tab = "scripts" | "files" | "runs";

const TABS: { id: Tab; label: string; icon: typeof FileCode }[] = [
  { id: "scripts", label: "Scripts", icon: FileCode },
  { id: "files", label: "Files", icon: FileIcon },
  { id: "runs", label: "Run history", icon: History },
];

type Dialog =
  | { kind: "script"; script: ToolboxScript | null }
  | { kind: "run"; scriptId?: string }
  | { kind: "upload" }
  | { kind: "file"; file: ToolboxFile }
  | { kind: "run-details"; run: ScriptRun };


// The toolbox's scripts and library files, and the runs of those scripts.
// Each item is private to the user who added it unless they share it.
export function ToolboxPanel() {
  const { authorizedFetch, inventory, can } = useWorkspace();
  const access = toolboxAccess(can);
  const tabs = TABS.filter(({ id }) => access.tabs.includes(id));
  const [tab, setTab] = useState<Tab>(access.tabs[0] ?? "scripts");
  const [toolbox, setToolbox] = useState<Toolbox | null>(null);
  const [loadError, setLoadError] = useState<string | null>(null);
  const [actionError, setActionError] = useState<string | null>(null);
  const [query, setQuery] = useState("");
  const [dialog, setDialog] = useState<Dialog | null>(null);
  const [busyId, setBusyId] = useState<string | null>(null);
  const [runsVersion, setRunsVersion] = useState(0);
  const opener = useRef<HTMLElement | null>(null);

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

  const open = (next: Dialog, event?: { currentTarget: HTMLElement }) => {
    opener.current = event?.currentTarget ?? null;
    setActionError(null);
    setDialog(next);
  };
  const close = () => setDialog(null);

  const scripts = useMemo(() => toolbox?.scripts ?? [], [toolbox]);
  const files = useMemo(() => toolbox?.files ?? [], [toolbox]);
  const folders = useMemo(() => folderSuggestions([...scripts, ...files]), [scripts, files]);

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
  const onTabKey = (index: number) => (event: KeyboardEvent<HTMLButtonElement>) => {
    const step = event.key === "ArrowRight" ? 1 : event.key === "ArrowLeft" ? -1 : 0;
    if (!step) return;
    event.preventDefault();
    const next = tabs[(index + step + tabs.length) % tabs.length].id;
    selectTab(next);
    document.getElementById(`toolbox-tab-${next}`)?.focus();
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
        <div className="panel-header">
          <div className="toolbox-tabs" role="tablist" aria-label="Toolbox">
            {tabs.map(({ id, label, icon: Icon }, index) => (
              <button
                key={id}
                id={`toolbox-tab-${id}`}
                type="button"
                role="tab"
                aria-selected={tab === id}
                aria-controls={`toolbox-${id}`}
                tabIndex={tab === id ? 0 : -1}
                onClick={() => selectTab(id)}
                onKeyDown={onTabKey(index)}
              >
                <Icon size={16} aria-hidden="true" />{label}
                {id === "scripts" && toolbox && <em>{scripts.length}</em>}
                {id === "files" && toolbox && <em>{files.length}</em>}
              </button>
            ))}
          </div>
          <div className="heading-actions">
            {tab === "scripts" && <>
              {access.runScripts && <button className="secondary-button" onClick={(event) => open({ kind: "run" }, event)} disabled={!scripts.length}><SquareTerminal size={16} /> Run a script</button>}
              {access.addScripts && <button className="primary-button" onClick={(event) => open({ kind: "script", script: null }, event)}><Plus size={16} /> New script</button>}
            </>}
            {tab === "files" && access.addFiles && <button className="primary-button" onClick={(event) => open({ kind: "upload" }, event)}><Upload size={16} /> Upload files</button>}
            {tab === "runs" && <button className="secondary-button" onClick={() => setRunsVersion((version) => version + 1)}><RefreshCw size={16} /> Refresh</button>}
          </div>
        </div>

        {tab !== "runs" && (
          <div className="table-toolbar">
            <label className="agent-search"><Search size={18} /><input aria-label={`Search ${tab}`} value={query} onChange={(event) => setQuery(event.target.value)} placeholder="Search by name, folder or description" /></label>
            <span className="toolbox-hint"><Lock size={14} aria-hidden="true" /> Private items are only yours. Shared items are everyone&apos;s.</span>
          </div>
        )}

        <div id="toolbox-scripts" role="tabpanel" aria-labelledby="toolbox-tab-scripts" hidden={tab !== "scripts"}>
          <ToolboxList
            loading={!toolbox}
            error={loadError}
            onRetry={() => void load()}
            items={visibleScripts}
            filtered={Boolean(query.trim())}
            empty={{ title: "No scripts yet", detail: "Write a PowerShell or Command Prompt script to run on your devices." }}
            renderRow={(script) => (
              <li key={script.id} className="toolbox-row">
                <FileCode size={18} className="toolbox-row-icon" aria-hidden="true" />
                <div className="toolbox-row-main">
                  <strong>{script.name}</strong>
                  <span>{LANGUAGE_LABELS[script.language]}{script.description ? ` · ${script.description}` : ""}</span>
                </div>
                <SharingBadge item={script} />
                <div className="row-actions">
                  {access.runScripts && <button className="remote-button" onClick={(event) => open({ kind: "run", scriptId: script.id }, event)}><Play size={15} /> Run</button>}
                  <button className="close-session-button" onClick={(event) => open({ kind: "script", script }, event)} aria-label={`${script.can_edit ? "Edit" : "View"} ${script.name}`} title={script.can_edit ? "Edit" : "View"}>{script.can_edit ? <Pencil size={16} /> : <Eye size={16} />}</button>
                  {script.can_edit && <button className="agent-delete-button" onClick={() => void removeScript(script)} disabled={busyId === script.id} aria-label={`Delete ${script.name}`} title="Delete">{busyId === script.id ? <LoaderCircle size={16} className="spin" /> : <Trash2 size={16} />}</button>}
                </div>
              </li>
            )}
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
            renderRow={(file) => (
              <li key={file.id} className="toolbox-row">
                <FileIcon size={18} className="toolbox-row-icon" aria-hidden="true" />
                <div className="toolbox-row-main">
                  <strong>{file.name}</strong>
                  <span>{formatBytes(file.size_bytes)}</span>
                </div>
                <SharingBadge item={file} />
                <div className="row-actions">
                  <button className="close-session-button" onClick={() => void download(file)} disabled={busyId === file.id} aria-label={`Download ${file.name}`} title="Download">{busyId === file.id ? <LoaderCircle size={16} className="spin" /> : <Download size={16} />}</button>
                  <button className="close-session-button" onClick={(event) => open({ kind: "file", file }, event)} aria-label={`${file.can_edit ? "Edit" : "View"} ${file.name}`} title={file.can_edit ? "Edit" : "Details"}>{file.can_edit ? <Pencil size={16} /> : <Eye size={16} />}</button>
                  {file.can_edit && <button className="agent-delete-button" onClick={() => void removeFile(file)} disabled={busyId === file.id} aria-label={`Delete ${file.name}`} title="Delete"><Trash2 size={16} /></button>}
                </div>
              </li>
            )}
          />
        </div>

        <div id="toolbox-runs" role="tabpanel" aria-labelledby="toolbox-tab-runs" hidden={tab !== "runs"}>
          {tab === "runs" && <RunHistory version={runsVersion} agents={inventory.agents} onOpen={(run, event) => open({ kind: "run-details", run }, event)} />}
        </div>
      </section>

      {dialog?.kind === "script" && (
        <ScriptEditor
          script={dialog.script}
          sharing={access.shareScripts ? (access.keepPrivateScripts ? "optional" : "required") : "unavailable"}
          folders={folders}
          onClose={close}
          onSaved={(saved, id) => { replaceScript(saved, id); close(); }}
          returnFocus={opener}
        />
      )}
      {dialog?.kind === "run" && (
        <RunScriptModal
          agents={inventory.agents}
          scripts={scripts}
          initialScriptId={dialog.scriptId}
          onClose={() => { close(); setRunsVersion((version) => version + 1); }}
          returnFocus={opener}
        />
      )}
      {dialog?.kind === "upload" && (
        <UploadFilesModal
          folders={folders}
          maxBytes={toolbox?.max_file_bytes ?? null}
          sharing={access.shareFiles ? (access.keepPrivateFiles ? "optional" : "required") : "unavailable"}
          onClose={close}
          onUploaded={(file) => replaceFile(file, file.id)}
          returnFocus={opener}
        />
      )}
      {dialog?.kind === "file" && (
        <FileDetailsModal
          file={dialog.file}
          canShare={access.shareFiles}
          canKeepPrivate={access.keepPrivateFiles}
          folders={folders}
          onClose={close}
          onSaved={(saved, id) => { replaceFile(saved, id); close(); }}
          returnFocus={opener}
        />
      )}
      {dialog?.kind === "run-details" && (
        <RunDetailsModal run={dialog.run} agents={inventory.agents} onClose={close} returnFocus={opener} />
      )}
    </>
  );
}

function SharingBadge({ item }: { item: { shared: boolean; owned: boolean } }) {
  return item.shared
    ? <span className="toolbox-badge toolbox-badge-shared" title="Everyone with access to the toolbox can see and use it"><Users size={13} aria-hidden="true" />{item.owned ? "Shared" : "Shared by a teammate"}</span>
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
            <td><strong>{run.script_name}</strong><span>{run.source === "session" ? "From a remote session" : "From the website"}{run.requested_by_you ? "" : " · by a teammate"}</span></td>
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

function RunDetailsModal({ run: listed, agents, onClose, returnFocus }: { run: ScriptRun; agents: Agent[]; onClose: () => void; returnFocus: { current: HTMLElement | null } }) {
  const { authorizedFetch } = useWorkspace();
  const { run, error } = useScriptRun(authorizedFetch, listed);
  const deviceName = agents.find((agent) => agent.id === listed.device_id)?.name ?? listed.device_id;
  return (
    <ModalDialog className="settings-modal toolbox-modal" labelledBy="run-details-title" onClose={onClose} returnFocus={returnFocus}>
      <button type="button" className="modal-close" onClick={onClose} aria-label="Close"><X size={19} /></button>
      <div className="modal-icon"><SquareTerminal size={22} /></div>
      <p className="eyebrow">Script run</p>
      <h2 id="run-details-title">{listed.script_name}</h2>
      {run && <RunResult run={run} deviceName={deviceName} error={error} />}
      <div className="connection-reason-actions"><button type="button" className="primary-button" onClick={onClose}>Done</button></div>
    </ModalDialog>
  );
}
