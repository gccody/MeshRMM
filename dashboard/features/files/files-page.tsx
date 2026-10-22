import {
  ArrowLeft,
  ArrowUp,
  CircleAlert,
  ClipboardCopy,
  Download,
  Eye,
  EyeOff,
  FileUp,
  FolderInput,
  FolderOpen,
  FolderPlus,
  FolderUp,
  HardDrive,
  House,
  LoaderCircle,
  MonitorOff,
  MoveRight,
  PencilLine,
  RefreshCw,
  Search,
  Trash2,
  WifiOff,
  X,
} from "lucide-react";
import { type FormEvent, type KeyboardEvent, useCallback, useEffect, useMemo, useRef, useState } from "react";
import { Link, useSearchParams } from "react-router";
import { AuthenticationRequired, RequestError, errorText } from "../../lib/http";
import type { Agent, DevicePlatform } from "../agents/types";
import { formatBytes } from "../metrics/model";
import { UsageBar } from "../metrics/usage-meter";
import { HeaderActions } from "../workspace/header-actions";
import { useWorkspace } from "../workspace/workspace-context";
import { type Chosen, DeleteDialog, MoveDialog, NewFolderDialog, RenameDialog } from "./file-dialogs";
import { FileList, type RowAction } from "./file-list";
import { createDownload, createFolder, deleteEntries, listFolder, listRoots, moveEntries, renameEntry } from "./files-api";
import {
  type FileAccount,
  type FileEntry,
  type FileRoots,
  type FileSort,
  PAGE_SIZE,
  accountLabel,
  accountOptions,
  crumbs,
  errorHelp,
  joinPath,
  pagesFor,
  parentPath,
} from "./model";
import { type Picked, fromDrop, fromFileList } from "./picked";
import { TransfersPanel, useTransfers } from "./transfers";

const HIDDEN_KEY = "meshrmm.files.hidden";

type Listing = {
  key: string;
  path: string;
  parent?: string;
  account: string;
  total: number;
  pages: Map<number, FileEntry[]>;
  readAt: number;
  truncated: boolean;
};

type Problem = { key: string; message: string; code?: string };

type Dialog =
  | { kind: "new-folder" }
  | { kind: "rename"; entry: Chosen }
  | { kind: "move"; entries: Chosen[] }
  | { kind: "delete"; entries: Chosen[] };

function useDebounced<T>(value: T, delay: number) {
  const [settled, setSettled] = useState(value);
  useEffect(() => {
    const timer = window.setTimeout(() => setSettled(value), delay);
    return () => window.clearTimeout(timer);
  }, [value, delay]);
  return settled;
}

function storedHidden() {
  try {
    return window.localStorage.getItem(HIDDEN_KEY) === "true";
  } catch {
    return false;
  }
}

const problemOf = (key: string, error: unknown, fallback: string): Problem => ({
  key,
  message: errorText(error, fallback),
  code: error instanceof RequestError ? error.code : undefined,
});

// `/files?id=…&path=…&as=…`: a device's folders and files. Without a path
// it starts at the drives or `/` and the users' folders.
export function FilesPage() {
  const { inventory, authorizedFetch, can, setPageTitle } = useWorkspace();
  const [params, setParams] = useSearchParams();
  const deviceId = params.get("id") ?? "";
  const path = params.get("path") ?? "";
  const agent = inventory.agents.find((candidate) => candidate.id === deviceId);
  const accounts = accountOptions(can);
  const requestedAccount = params.get("as");
  const account: FileAccount = accounts.find((option) => option === requestedAccount) ?? accounts[0] ?? "user";

  useEffect(() => {
    setPageTitle(agent ? `Files on ${agent.name}` : null);
    return () => setPageTitle(null);
  }, [agent, setPageTitle]);

  if (inventory.hasData && !agent) {
    return (
      <div className="empty-state">
        <MonitorOff size={22} aria-hidden="true" />
        <strong>Device not found</strong>
        <span>It may have been deleted.</span>
        <Link className="secondary-button" to="/">Back to devices</Link>
      </div>
    );
  }
  if (!agent) {
    return <div className="empty-state compact"><LoaderCircle size={22} className="spin" aria-hidden="true" /><strong>Loading the device…</strong></div>;
  }
  return (
    <FileBrowser
      key={agent.id}
      agent={agent}
      account={account}
      accounts={accounts}
      path={path}
      authorizedFetch={authorizedFetch}
      canManage={can("files.manage")}
      navigate={(next, as = account) => {
        const query = new URLSearchParams({ id: agent.id });
        if (next) query.set("path", next);
        query.set("as", as);
        setParams(query);
      }}
    />
  );
}

type BrowserProps = {
  agent: Agent;
  account: FileAccount;
  accounts: FileAccount[];
  path: string;
  authorizedFetch: ReturnType<typeof useWorkspace>["authorizedFetch"];
  canManage: boolean;
  navigate: (path: string, account?: FileAccount) => void;
};

function FileBrowser({ agent, account, accounts, path, authorizedFetch, canManage, navigate }: BrowserProps) {
  const platform = agent.platform;
  const connected = agent.connected;
  const [sort, setSort] = useState<FileSort>("name");
  const [descending, setDescending] = useState(false);
  const [hidden, setHidden] = useState(storedHidden);
  const [filterText, setFilterText] = useState("");
  const filter = useDebounced(filterText, 300);
  const [reload, setReload] = useState(0);
  const [roots, setRoots] = useState<FileRoots | null>(null);
  const [rootsProblem, setRootsProblem] = useState<string | null>(null);
  const [listing, setListing] = useState<Listing | null>(null);
  const [problem, setProblem] = useState<Problem | null>(null);
  // The entries chosen in a listing, by name, with their rows.
  const [chosenIn, setChosenIn] = useState<{ key: string; rows: Map<string, number> }>({ key: "", rows: new Map() });
  const anchor = useRef(0);
  const [dialog, setDialog] = useState<Dialog | null>(null);
  const [notice, setNotice] = useState<{ text: string; error?: boolean } | null>(null);
  // The path being typed, for the folder it was started in.
  const [pathEdit, setPathEdit] = useState<{ path: string; text: string } | null>(null);
  const pathInput = useRef<HTMLInputElement>(null);
  const filesInput = useRef<HTMLInputElement>(null);
  const folderInput = useRef<HTMLInputElement>(null);
  const opener = useRef<HTMLButtonElement>(null);
  const inflight = useRef(new Set<string>());

  const listKey = `${account}|${path}|${sort}|${descending}|${hidden}|${filter}|${reload}`;
  const currentKey = useRef(listKey);
  useEffect(() => {
    currentKey.current = listKey;
  });

  const refresh = useCallback(() => setReload((count) => count + 1), []);

  // The device coming back online picks up where the page was.
  const wasConnected = useRef(connected);
  useEffect(() => {
    if (connected && !wasConnected.current) refresh();
    wasConnected.current = connected;
  }, [connected, refresh]);

  useEffect(() => {
    if (!connected) return;
    let cancelled = false;
    listRoots(authorizedFetch, agent.id, account).then(
      (loaded) => {
        if (!cancelled) {
          setRoots(loaded);
          setRootsProblem(null);
        }
      },
      (error) => {
        if (!cancelled && !(error instanceof AuthenticationRequired)) setRootsProblem(errorText(error, "The device’s drives couldn’t be listed."));
      },
    );
    return () => {
      cancelled = true;
    };
  }, [authorizedFetch, agent.id, account, connected, reload]);

  const loadPage = useCallback((key: string, page: number) => {
    const id = `${key}#${page}`;
    if (inflight.current.has(id)) return;
    inflight.current.add(id);
    listFolder(authorizedFetch, agent.id, {
      path,
      account,
      offset: page * PAGE_SIZE,
      limit: PAGE_SIZE,
      sort,
      descending,
      hidden,
      filter,
      // A listing starts with a fresh reading; its other pages come from it.
      refresh: page === 0,
    })
      .then(
        (result) => {
          if (currentKey.current !== key) return;
          setListing((current) => {
            const pages = new Map(current?.key === key ? current.pages : undefined);
            pages.set(page, result.entries);
            return {
              key,
              path: result.path,
              parent: result.parent,
              account: result.account,
              total: result.total,
              pages,
              readAt: result.read_at,
              truncated: Boolean(result.truncated),
            };
          });
        },
        (error: unknown) => {
          if (currentKey.current === key && page === 0 && !(error instanceof AuthenticationRequired)) {
            setProblem(problemOf(key, error, "The folder couldn’t be listed."));
          }
        },
      )
      .finally(() => inflight.current.delete(id));
  }, [authorizedFetch, agent.id, path, account, sort, descending, hidden, filter]);

  useEffect(() => {
    if (path && connected) loadPage(listKey, 0);
  }, [listKey, path, connected, loadPage]);

  const selection = useMemo(() => (chosenIn.key === listKey ? chosenIn.rows : new Map<string, number>()), [chosenIn, listKey]);
  const setSelection = useCallback((change: Map<string, number> | ((current: Map<string, number>) => Map<string, number>)) => {
    setChosenIn((current) => {
      const rows = current.key === listKey ? current.rows : new Map<string, number>();
      return { key: listKey, rows: typeof change === "function" ? change(rows) : change };
    });
  }, [listKey]);
  const editingPath = pathEdit?.path === path;
  useEffect(() => {
    if (editingPath) pathInput.current?.focus();
  }, [editingPath]);

  const shown = listing?.key === listKey ? listing : null;
  const folder = shown?.path ?? path;
  const entryAt = useCallback(
    (index: number) => shown?.pages.get(Math.floor(index / PAGE_SIZE))?.[index % PAGE_SIZE],
    [shown],
  );
  const onVisible = useCallback((first: number, last: number) => {
    if (!shown) return;
    for (const page of pagesFor(first, last)) {
      if (!shown.pages.has(page)) loadPage(shown.key, page);
    }
  }, [shown, loadPage]);

  const chosen = useMemo(() => {
    const entries: Chosen[] = [];
    for (const index of selection.values()) {
      const entry = entryAt(index);
      if (entry) entries.push({ ...entry, path: joinPath(platform, folder, entry.name) });
    }
    return entries;
  }, [selection, entryAt, platform, folder]);

  const loadedCount = shown ? [...shown.pages.values()].reduce((count, page) => count + page.length, 0) : 0;

  const select = useCallback((index: number, mode: "only" | "toggle" | "range") => {
    const entry = entryAt(index);
    if (!entry) return;
    setSelection((current) => {
      if (mode === "only") {
        anchor.current = index;
        return new Map([[entry.name, index]]);
      }
      if (mode === "toggle") {
        anchor.current = index;
        const next = new Map(current);
        if (next.has(entry.name)) next.delete(entry.name);
        else next.set(entry.name, index);
        return next;
      }
      const next = new Map<string, number>();
      const [from, to] = anchor.current <= index ? [anchor.current, index] : [index, anchor.current];
      for (let row = from; row <= to; row++) {
        const candidate = entryAt(row);
        if (candidate) next.set(candidate.name, row);
      }
      return next;
    });
  }, [entryAt, setSelection]);

  const selectAll = (all: boolean) => {
    if (!all || !shown) {
      setSelection(new Map());
      return;
    }
    const next = new Map<string, number>();
    for (const [page, entries] of shown.pages) entries.forEach((entry, offset) => next.set(entry.name, page * PAGE_SIZE + offset));
    setSelection(next);
  };

  const flash = (text: string, error = false) => setNotice({ text, error });

  const onLanded = useCallback((landed: string) => {
    if (landed === folder) refresh();
  }, [folder, refresh]);
  const transfers = useTransfers({ authorizedFetch, deviceId: agent.id, platform, connected, onLanded });

  const go = (next: string) => {
    setFilterText("");
    navigate(next);
  };

  const download = async (entries: Chosen[]) => {
    if (!entries.length) return;
    try {
      const view = await createDownload(authorizedFetch, agent.id, account, entries.map((entry) => entry.path));
      transfers.addDownload(view);
    } catch (error) {
      if (!(error instanceof AuthenticationRequired)) flash(errorText(error, "The download couldn’t start."), true);
    }
  };

  const open = (entry: FileEntry) => {
    const target = joinPath(platform, folder, entry.name);
    if (entry.kind === "directory") go(target);
    else if (entry.kind === "file") void download([{ ...entry, path: target }]);
  };

  const copyPath = (text: string) => {
    navigator.clipboard?.writeText(text).then(
      () => flash(`Copied ${text}`),
      () => flash("The browser didn’t allow copying.", true),
    );
  };

  const upload = (picked: Picked, into = folder) => {
    if (!picked.files.length && !picked.folders.length) return;
    void transfers.enqueue(picked, into, account);
  };

  const actionsFor = (entry: FileEntry): RowAction[] => {
    const target: Chosen = { ...entry, path: joinPath(platform, folder, entry.name) };
    const actions: RowAction[] = [];
    if (entry.kind === "directory") actions.push({ label: "Open", icon: <FolderOpen size={16} aria-hidden="true" />, onSelect: () => go(target.path) });
    if (entry.kind !== "other") actions.push({ label: entry.kind === "directory" ? "Download as ZIP" : "Download", icon: <Download size={16} aria-hidden="true" />, onSelect: () => void download([target]) });
    if (canManage) {
      actions.push(
        { label: "Rename", icon: <PencilLine size={16} aria-hidden="true" />, onSelect: () => setDialog({ kind: "rename", entry: target }) },
        { label: "Move to…", icon: <MoveRight size={16} aria-hidden="true" />, onSelect: () => setDialog({ kind: "move", entries: [target] }) },
      );
    }
    actions.push({ label: "Copy path", icon: <ClipboardCopy size={16} aria-hidden="true" />, onSelect: () => copyPath(target.path) });
    if (canManage) actions.push({ label: "Delete…", icon: <Trash2 size={16} aria-hidden="true" />, onSelect: () => setDialog({ kind: "delete", entries: [target] }), danger: true });
    return actions;
  };

  const listKeys = (event: KeyboardEvent<HTMLElement>) => {
    if (event.key === "Delete" && canManage && chosen.length) {
      event.preventDefault();
      setDialog({ kind: "delete", entries: chosen });
    } else if (event.key === "F2" && canManage && chosen.length === 1) {
      event.preventDefault();
      setDialog({ kind: "rename", entry: chosen[0] });
    } else if (event.key === "Backspace" && shown?.parent !== undefined) {
      event.preventDefault();
      go(shown.parent);
    } else if (event.key.toLowerCase() === "a" && (event.metaKey || event.ctrlKey)) {
      event.preventDefault();
      selectAll(true);
    }
  };

  const submitPath = (event: FormEvent) => {
    event.preventDefault();
    const next = pathEdit?.text.trim() ?? "";
    setPathEdit(null);
    if (next && next !== path) go(next);
  };

  const otherAccount = accounts.find((option) => option !== account);
  const suggestions = [...new Set([folder, ...(roots?.roots.map((root) => root.path) ?? []), ...(roots?.places.map((place) => place.path) ?? [])])];
  const trail = path ? crumbs(platform, folder) : [];
  const offline = !connected;

  return (
    <>
      {accounts.length > 1 && (
        <HeaderActions>
          <div className="segmented" role="group" aria-label="Browse as">
            {accounts.map((option) => (
              <button key={option} type="button" aria-pressed={option === account} onClick={() => navigate(path, option)} title={option === "user" ? "The person signed in to the device; their own permissions apply" : "The device’s own account, which reaches every file"}>
                {accountLabel(option, platform)}
              </button>
            ))}
          </div>
        </HeaderActions>
      )}
      <div className="files-page">
        <div className="device-status">
          <Link to={`/device?id=${encodeURIComponent(agent.id)}`} className="back-link"><ArrowLeft size={15} aria-hidden="true" />{agent.name}</Link>
          <p className="status-line">
            <i className={`led ${connected ? "online" : agent.updating_to ? "updating" : "offline"}`} aria-hidden="true" />
            <span>{connected ? "Online" : agent.updating_to ? `Updating to ${agent.updating_to}` : "Offline"}</span>
            <span aria-hidden="true">·</span>
            <span>As {shown?.account ?? roots?.account ?? accountLabel(account, platform)}</span>
          </p>
        </div>

        {offline && (
          <div className="error-banner" role="status"><WifiOff size={16} aria-hidden="true" /><span>The device is offline. Its files show again when it’s back online{transfers.items.length ? ", and paused uploads go on" : ""}.</span></div>
        )}
        {notice && (
          <div className={notice.error ? "error-banner" : "file-notice"} role={notice.error ? "alert" : "status"}>
            {notice.error && <CircleAlert size={16} aria-hidden="true" />}<span>{notice.text}</span>
            <button type="button" onClick={() => setNotice(null)} aria-label="Dismiss"><X size={15} /></button>
          </div>
        )}

        <div className="files-layout">
          <Places roots={roots} problem={rootsProblem} platform={platform} current={folder} onGo={go} onStart={() => go("")} />

          <section className="files-main" aria-label="Folder">
            <div className="file-pathbar">
              <button type="button" className="icon-button" onClick={() => go(shown?.parent ?? parentPath(platform, path) ?? "")} disabled={!path} aria-label="Up one folder" title="Up one folder"><ArrowUp size={16} /></button>
              <button type="button" className="icon-button" onClick={refresh} disabled={offline} aria-label="Refresh" title="Refresh"><RefreshCw size={16} /></button>
              {editingPath ? (
                <form className="file-path-form" onSubmit={submitPath}>
                  <input ref={pathInput} value={pathEdit?.text ?? ""} onChange={(event) => setPathEdit({ path, text: event.target.value })} onKeyDown={(event) => event.key === "Escape" && setPathEdit(null)} aria-label="Folder path" spellCheck={false} autoComplete="off" placeholder={platform === "windows" ? "C:\\Users" : "/home"} />
                  <button className="secondary-button">Go</button>
                </form>
              ) : (
                <nav className="file-crumbs" aria-label="Folder path">
                  <ol>
                    <li><button type="button" onClick={() => go("")} aria-current={!path ? "page" : undefined}><HardDrive size={14} aria-hidden="true" />{platform === "windows" ? "This PC" : "Start"}</button></li>
                    {trail.map((crumb, index) => (
                      <li key={crumb.path}><button type="button" onClick={() => go(crumb.path)} aria-current={index === trail.length - 1 ? "page" : undefined}>{crumb.label}</button></li>
                    ))}
                  </ol>
                  <button type="button" className="file-crumbs-edit" onClick={() => setPathEdit({ path, text: folder })} aria-label="Type a path">
                    <PencilLine size={14} aria-hidden="true" />
                  </button>
                </nav>
              )}
            </div>

            {path ? (
              <>
                <div className="file-toolbar" role="toolbar" aria-label="File actions">
                  {canManage && <>
                    <button type="button" className="secondary-button" onClick={() => filesInput.current?.click()} disabled={offline}><FileUp size={15} aria-hidden="true" />Upload files</button>
                    <button type="button" className="secondary-button" onClick={() => folderInput.current?.click()} disabled={offline}><FolderUp size={15} aria-hidden="true" />Upload folder</button>
                    <button ref={opener} type="button" className="secondary-button" onClick={() => setDialog({ kind: "new-folder" })} disabled={offline}><FolderPlus size={15} aria-hidden="true" />New folder</button>
                    <input ref={filesInput} type="file" multiple hidden onChange={(event) => { upload(fromFileList(event.target.files)); event.target.value = ""; }} />
                    <input ref={folderInput} type="file" hidden {...{ webkitdirectory: "" }} onChange={(event) => { upload(fromFileList(event.target.files)); event.target.value = ""; }} />
                  </>}
                  <span className="file-toolbar-group" aria-label="Selected">
                    <button type="button" className="secondary-button" disabled={!chosen.length || offline} onClick={() => void download(chosen)}><Download size={15} aria-hidden="true" />Download</button>
                    {canManage && <>
                      <button type="button" className="secondary-button" disabled={chosen.length !== 1 || offline} onClick={() => setDialog({ kind: "rename", entry: chosen[0] })}><PencilLine size={15} aria-hidden="true" />Rename</button>
                      <button type="button" className="secondary-button" disabled={!chosen.length || offline} onClick={() => setDialog({ kind: "move", entries: chosen })}><FolderInput size={15} aria-hidden="true" />Move</button>
                      <button type="button" className="danger-button" disabled={!chosen.length || offline} onClick={() => setDialog({ kind: "delete", entries: chosen })}><Trash2 size={15} aria-hidden="true" />Delete</button>
                    </>}
                    <button type="button" className="icon-button file-copy-path" onClick={() => copyPath(chosen.length === 1 ? chosen[0].path : folder)} title={chosen.length === 1 ? "Copy the selected entry’s path" : "Copy this folder’s path"} aria-label={chosen.length === 1 ? "Copy the selected entry’s path" : "Copy this folder’s path"}><ClipboardCopy size={15} /></button>
                  </span>
                  <span className="file-toolbar-end">
                    <label className="search-field file-filter">
                      <Search size={15} aria-hidden="true" />
                      <input type="search" value={filterText} onChange={(event) => setFilterText(event.target.value)} placeholder="Filter this folder" aria-label="Filter this folder by name" />
                    </label>
                    <button
                      type="button"
                      className="secondary-button"
                      aria-pressed={hidden}
                      onClick={() => {
                        setHidden(!hidden);
                        try {
                          window.localStorage.setItem(HIDDEN_KEY, String(!hidden));
                        } catch {
                          // Not remembering is fine.
                        }
                      }}
                      title={platform === "windows" ? "Show hidden and system files" : "Show hidden files"}
                    >
                      {hidden ? <Eye size={15} aria-hidden="true" /> : <EyeOff size={15} aria-hidden="true" />}Hidden
                    </button>
                  </span>
                </div>

                {problem?.key === listKey ? (
                  <FolderProblem
                    problem={problem}
                    platform={platform}
                    account={account}
                    parent={parentPath(platform, path)}
                    other={otherAccount}
                    onUp={(parent) => go(parent)}
                    onRetry={refresh}
                    onSwitch={(other) => navigate(path, other)}
                  />
                ) : !shown ? (
                  <div className="empty-state compact">
                    {offline ? <WifiOff size={22} aria-hidden="true" /> : <LoaderCircle size={22} className="spin" aria-hidden="true" />}
                    <strong>{offline ? "The device is offline" : "Reading the folder…"}</strong>
                  </div>
                ) : (
                  <>
                    {shown.total === 0 ? (
                      <div
                        className="empty-state compact file-empty"
                        onDragOver={(event) => { if (canManage) event.preventDefault(); }}
                        onDrop={(event) => {
                          if (!canManage) return;
                          event.preventDefault();
                          void fromDrop(event.dataTransfer).then((picked) => upload(picked));
                        }}
                      >
                        <FolderOpen size={22} aria-hidden="true" />
                        <strong>{filter ? "Nothing here matches" : "This folder is empty"}</strong>
                        {canManage && !filter && <span>Drop files or folders here to upload them.</span>}
                      </div>
                    ) : (
                      <FileList
                        listKey={listKey}
                        total={shown.total}
                        entryAt={entryAt}
                        sort={sort}
                        descending={descending}
                        onSort={(column) => {
                          if (column === sort) setDescending(!descending);
                          else {
                            setSort(column);
                            setDescending(column === "modified" || column === "size");
                          }
                        }}
                        selected={new Set(selection.keys())}
                        onSelect={select}
                        allSelected={selection.size > 0 && selection.size === shown.total}
                        onSelectAll={selectAll}
                        onOpen={open}
                        actionsFor={actionsFor}
                        onVisible={onVisible}
                        onDropFiles={canManage && !offline ? (transfer, into) => {
                          const target = into ? joinPath(platform, folder, into.name) : folder;
                          void fromDrop(transfer).then((picked) => upload(picked, target));
                        } : undefined}
                        onKeyDown={listKeys}
                        label={`Contents of ${folder}`}
                      />
                    )}
                    <p className="file-status" role="status">
                      {shown.total.toLocaleString()} {shown.total === 1 ? "item" : "items"}{filter ? ` matching “${filter}”` : ""}
                      {selection.size > 0 && ` · ${selection.size.toLocaleString()} selected`}
                      {loadedCount < shown.total && selection.size > 0 && " (of those loaded)"}
                      {shown.truncated && " · The folder has more entries than the Agent lists; filter by name to find the rest."}
                      <span> · Read {new Date(shown.readAt).toLocaleTimeString([], { hour: "numeric", minute: "2-digit", second: "2-digit" })}</span>
                    </p>
                  </>
                )}
              </>
            ) : (
              <StartView roots={roots} problem={rootsProblem} offline={offline} platform={platform} onGo={go} />
            )}

            <TransfersPanel transfers={transfers} platformLabel={agent.name} />
          </section>
        </div>
      </div>

      {dialog?.kind === "new-folder" && (
        <NewFolderDialog
          platform={platform}
          returnFocus={opener}
          onClose={() => setDialog(null)}
          onCreate={async (name) => {
            await createFolder(authorizedFetch, agent.id, account, joinPath(platform, folder, name));
            refresh();
          }}
        />
      )}
      {dialog?.kind === "rename" && (
        <RenameDialog
          platform={platform}
          entry={dialog.entry}
          onClose={() => setDialog(null)}
          onRename={async (name) => {
            await renameEntry(authorizedFetch, agent.id, account, dialog.entry.path, name);
            refresh();
          }}
        />
      )}
      {dialog?.kind === "move" && (
        <MoveDialog
          platform={platform}
          entries={dialog.entries}
          folder={folder}
          suggestions={suggestions}
          onClose={() => setDialog(null)}
          onMove={async (paths, destination) => {
            const batch = await moveEntries(authorizedFetch, agent.id, account, paths, destination);
            if (batch.done.length) refresh();
            return batch;
          }}
        />
      )}
      {dialog?.kind === "delete" && (
        <DeleteDialog
          platform={platform}
          entries={dialog.entries}
          account={account}
          onClose={() => setDialog(null)}
          onDelete={async (paths, trash) => {
            const batch = await deleteEntries(authorizedFetch, agent.id, account, paths, trash);
            if (batch.done.length) refresh();
            return batch;
          }}
        />
      )}
    </>
  );
}

// Why the folder isn't shown, and the ways on.
function FolderProblem({ problem, platform, account, parent, other, onUp, onRetry, onSwitch }: {
  problem: Problem;
  platform: DevicePlatform;
  account: FileAccount;
  parent: string | null;
  other: FileAccount | undefined;
  onUp: (parent: string) => void;
  onRetry: () => void;
  onSwitch: (account: FileAccount) => void;
}) {
  const help = errorHelp(problem.code, account, platform);
  const switchable = other && (problem.code === "access_denied" || problem.code === "no_user");
  const title = problem.code === "access_denied" ? "Access denied"
    : problem.code === "not_found" ? "Not found"
    : problem.code === "device_offline" || problem.code === "device_disconnected" ? "The device is offline"
    : problem.code === "no_user" ? "Nobody is signed in"
    : "The folder couldn’t be shown";
  return (
    <div className="empty-state compact file-problem" role="alert">
      <CircleAlert size={22} aria-hidden="true" />
      <strong>{title}</strong>
      <span>{problem.message}{help ? ` ${help}` : ""}</span>
      <span className="file-problem-actions">
        {parent && <button type="button" className="secondary-button" onClick={() => onUp(parent)}><ArrowUp size={15} aria-hidden="true" />Up one folder</button>}
        {switchable && <button type="button" className="secondary-button" onClick={() => onSwitch(other)}>Browse as {accountLabel(other, platform)}</button>}
        <button type="button" className="secondary-button" onClick={onRetry}><RefreshCw size={15} aria-hidden="true" />Try again</button>
      </span>
    </div>
  );
}

const PLACE_LABELS = { home: "Home", desktop: "Desktop", documents: "Documents", downloads: "Downloads" } as const;

function Places({ roots, problem, platform, current, onGo, onStart }: {
  roots: FileRoots | null;
  problem: string | null;
  platform: DevicePlatform;
  current: string;
  onGo: (path: string) => void;
  onStart: () => void;
}) {
  return (
    <nav className="file-places" aria-label="Places">
      <button type="button" onClick={onStart} aria-current={!current ? "page" : undefined}><HardDrive size={15} aria-hidden="true" />{platform === "windows" ? "This PC" : "Start"}</button>
      {problem && <p className="field-help">{problem}</p>}
      {roots && roots.roots.length > 0 && <>
        <h3>{platform === "windows" ? "Drives" : "Volumes"}</h3>
        {roots.roots.map((root) => (
          <button key={root.path} type="button" onClick={() => onGo(root.path)} aria-current={current === root.path ? "page" : undefined} title={root.path}>
            <HardDrive size={15} aria-hidden="true" /><span>{root.label}</span>
          </button>
        ))}
      </>}
      {roots && roots.places.length > 0 && <>
        <h3>Folders</h3>
        {roots.places.map((place) => (
          <button key={`${place.user}|${place.path}`} type="button" onClick={() => onGo(place.path)} aria-current={current === place.path ? "page" : undefined} title={place.path}>
            {place.kind === "home" ? <House size={15} aria-hidden="true" /> : <FolderOpen size={15} aria-hidden="true" />}
            <span>{PLACE_LABELS[place.kind]}<small>{place.user}</small></span>
          </button>
        ))}
      </>}
    </nav>
  );
}

// Where browsing starts: drives or volumes with their space, and the
// users' folders.
function StartView({ roots, problem, offline, platform, onGo }: {
  roots: FileRoots | null;
  problem: string | null;
  offline: boolean;
  platform: DevicePlatform;
  onGo: (path: string) => void;
}) {
  if (offline && !roots) return <div className="empty-state compact"><WifiOff size={22} aria-hidden="true" /><strong>The device is offline</strong></div>;
  if (problem && !roots) return <div className="empty-state compact" role="alert"><CircleAlert size={22} aria-hidden="true" /><strong>The device’s drives couldn’t be listed</strong><span>{problem}</span></div>;
  if (!roots) return <div className="empty-state compact"><LoaderCircle size={22} className="spin" aria-hidden="true" /><strong>Finding the device’s drives…</strong></div>;
  return (
    <div className="file-start">
      <h2>{platform === "windows" ? "Drives" : "Volumes"}</h2>
      <ul className="file-roots">
        {roots.roots.map((root) => {
          const used = root.total_bytes && root.free_bytes !== undefined ? root.total_bytes - root.free_bytes : null;
          return (
            <li key={root.path}>
              <button type="button" onClick={() => onGo(root.path)}>
                <HardDrive size={20} aria-hidden="true" />
                <span><strong>{root.label}</strong>
                  {used !== null && root.total_bytes ? <>
                    <UsageBar percent={(used / root.total_bytes) * 100} />
                    <small>{formatBytes(root.free_bytes ?? 0)} free of {formatBytes(root.total_bytes)}</small>
                  </> : <small>{root.path}</small>}
                </span>
              </button>
            </li>
          );
        })}
      </ul>
      {roots.places.length > 0 && <>
        <h2>Folders</h2>
        <ul className="file-roots">
          {roots.places.map((place) => (
            <li key={`${place.user}|${place.path}`}>
              <button type="button" onClick={() => onGo(place.path)}>
                {place.kind === "home" ? <House size={20} aria-hidden="true" /> : <FolderOpen size={20} aria-hidden="true" />}
                <span><strong>{PLACE_LABELS[place.kind]}</strong><small>{place.user} · {place.path}</small></span>
              </button>
            </li>
          ))}
        </ul>
      </>}
    </div>
  );
}
