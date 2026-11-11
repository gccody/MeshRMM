import { ArrowUp, CircleAlert, FilePlus2, Folder, FolderPlus, FolderTree, LoaderCircle, PencilLine, RefreshCw, Save, Search, Trash2, X } from "lucide-react";
import { type FormEvent, type ReactNode, type RefObject, useEffect, useId, useMemo, useRef, useState } from "react";
import { AuthenticationRequired, RequestError, errorText } from "../../lib/http";
import { formatDateTime } from "../../lib/format";
import { ModalDialog } from "../../lib/modal-dialog";
import type { Agent } from "../agents/types";
import { useWorkspace } from "../workspace/workspace-context";
import { errorHelp } from "./lists";
import {
  type NumberBase,
  REGISTRY_ROOTS,
  ROOT_NAMES,
  type RegistryKey,
  type RegistryListing,
  type RegistryRoot,
  type RegistrySubkey,
  type RegistryType,
  type RegistryValue,
  type RegistryView,
  TYPE_NAMES,
  WRITABLE_TYPES,
  childPath,
  dataFromDraft,
  draftOf,
  formatData,
  freshName,
  keyDisplay,
  keyNameProblem,
  parentPath,
  parseKeyPath,
  pathNames,
  rebase,
  sortedValues,
  typeName,
} from "./registry";
import {
  createRegistryKey,
  deleteRegistryKey,
  deleteRegistryValue,
  getRegistryValue,
  listRegistryKey,
  renameRegistryValue,
  setRegistryValue,
} from "./tools-api";

// The most subkeys or values a table draws; a filter narrows the rest.
const MAX_ROWS = 2000;

type Problem = { message: string; code?: string };

const problemOf = (error: unknown, fallback: string): Problem => ({
  message: errorText(error, fallback),
  code: error instanceof RequestError ? error.code : undefined,
});

type Dialog =
  | { kind: "new-key" }
  | { kind: "delete-key"; subkey: RegistrySubkey }
  | { kind: "edit-value"; value: RegistryValue | null }
  | { kind: "rename-value"; value: RegistryValue }
  | { kind: "delete-value"; value: RegistryValue };

const valueLabel = (name: string) => (name === "" ? "(Default)" : name);

// A Windows device's registry: a key's subkeys and values, browsed and
// changed as SYSTEM.
export function RegistryPanel({ agent, canManage }: { agent: Agent; canManage: boolean }) {
  const { authorizedFetch } = useWorkspace();
  const [key, setKey] = useState<RegistryKey>({ root: "HKLM", path: "", view: "64" });
  const [listing, setListing] = useState<RegistryListing | null>(null);
  const [problem, setProblem] = useState<Problem | null>(null);
  const [reload, setReload] = useState(0);
  // The key and reload the latest answer, or failure, was for.
  const [answered, setAnswered] = useState<string | null>(null);
  const asked = `${key.root}|${key.view}|${key.path}|${reload}`;
  const loading = answered !== asked;
  const [pathEdit, setPathEdit] = useState<string | null>(null);
  const [filter, setFilter] = useState("");
  const [dialog, setDialog] = useState<Dialog | null>(null);
  const [notice, setNotice] = useState<string | null>(null);
  const opener = useRef<HTMLElement | null>(null);
  const pathInput = useRef<HTMLInputElement>(null);
  const editingPath = pathEdit !== null;
  useEffect(() => {
    if (editingPath) pathInput.current?.select();
  }, [editingPath]);

  useEffect(() => {
    const controller = new AbortController();
    listRegistryKey(authorizedFetch, agent.id, key, controller.signal).then(
      (next) => {
        setListing(next);
        setProblem(null);
        setAnswered(asked);
      },
      (error: unknown) => {
        if (controller.signal.aborted || error instanceof AuthenticationRequired) return;
        setProblem(problemOf(error, "The key couldn’t be opened."));
        setAnswered(asked);
      },
    );
    return () => controller.abort();
  }, [authorizedFetch, agent.id, key, asked]);

  const go = (next: Partial<RegistryKey>) => {
    setKey((current) => ({ ...current, ...next }));
    setFilter("");
    setNotice(null);
    setPathEdit(null);
  };
  const refresh = () => setReload((count) => count + 1);
  const open = (event: { currentTarget: HTMLElement }, next: Dialog) => {
    opener.current = event.currentTarget;
    setNotice(null);
    setDialog(next);
  };
  const done = (text: string) => {
    setDialog(null);
    setNotice(text);
    refresh();
  };

  // What's shown is the listing of the key asked for; another key's, while
  // the next loads, would mislead.
  const shown = listing && listing.key.root === key.root && listing.key.view === key.view && listing.key.path === key.path ? listing : null;
  const needle = filter.trim().toLowerCase();
  const subkeys = useMemo(
    () => (shown?.subkeys ?? []).filter((subkey) => subkey.name.toLowerCase().includes(needle)),
    [shown, needle],
  );
  const values = useMemo(
    () => sortedValues(shown?.values ?? []).filter((value) => !needle || value.name.toLowerCase().includes(needle) || formatData(value).toLowerCase().includes(needle)),
    [shown, needle],
  );
  const hasDefault = shown?.values.some((value) => value.name === "") ?? false;
  const names = pathNames(key.path);

  const submitPath = (event: FormEvent) => {
    event.preventDefault();
    const parsed = parseKeyPath(pathEdit ?? "");
    if (!parsed) {
      setProblem({ message: "Start the path with a root: HKEY_LOCAL_MACHINE, HKEY_USERS, HKEY_CLASSES_ROOT or HKEY_CURRENT_CONFIG. HKEY_CURRENT_USER is each user’s key under HKEY_USERS." });
      return;
    }
    go(parsed);
  };

  return (
    <section className="agent-panel tools-panel registry-panel" aria-label="Registry">
      <div className="table-toolbar">
        <div className="segmented" role="group" aria-label="Root key">
          {REGISTRY_ROOTS.map((root: RegistryRoot) => (
            <button key={root} type="button" aria-pressed={key.root === root} title={ROOT_NAMES[root]} onClick={() => go({ root, path: "" })}>{root}</button>
          ))}
        </div>
        <div className="segmented" role="group" aria-label="Registry view">
          {(["64", "32"] as RegistryView[]).map((view) => (
            <button key={view} type="button" aria-pressed={key.view === view} title={view === "64" ? "The registry 64-bit programs see" : "The registry 32-bit programs see, where SOFTWARE is WOW6432Node"} onClick={() => go({ view })}>{view}-bit</button>
          ))}
        </div>
        <label className="search-field">
          <Search size={16} aria-hidden="true" />
          <input type="search" aria-label="Filter this key" value={filter} onChange={(event) => setFilter(event.target.value)} placeholder="Filter this key" />
        </label>
        {canManage && (
          <div className="selection-actions">
            <button type="button" className="secondary-button" onClick={(event) => open(event, { kind: "new-key" })}><FolderPlus size={15} aria-hidden="true" />New key</button>
            <button type="button" className="secondary-button" disabled={!shown} onClick={(event) => open(event, { kind: "edit-value", value: null })}><FilePlus2 size={15} aria-hidden="true" />New value</button>
          </div>
        )}
      </div>
      <div className="file-pathbar registry-pathbar">
        <button type="button" className="icon-button" onClick={() => go({ path: parentPath(key.path) })} disabled={!names.length} aria-label="Up one key" title="Up one key"><ArrowUp size={16} /></button>
        <button type="button" className="icon-button" onClick={refresh} disabled={loading} aria-label="Refresh" title="Refresh">{loading ? <LoaderCircle size={16} className="spin" /> : <RefreshCw size={16} />}</button>
        {pathEdit !== null ? (
          <form className="file-path-form" onSubmit={submitPath}>
            <input ref={pathInput} value={pathEdit} onChange={(event) => setPathEdit(event.target.value)} onKeyDown={(event) => event.key === "Escape" && setPathEdit(null)} aria-label="Key path" spellCheck={false} autoComplete="off" placeholder="HKEY_LOCAL_MACHINE\SOFTWARE" />
            <button className="secondary-button">Go</button>
          </form>
        ) : (
          <nav className="file-crumbs" aria-label="Key path">
            <ol>
              <li><button type="button" onClick={() => go({ path: "" })} aria-current={!names.length ? "page" : undefined}><FolderTree size={14} aria-hidden="true" />{ROOT_NAMES[key.root]}</button></li>
              {names.map((name, index) => (
                <li key={`${index}-${name}`}><button type="button" onClick={() => go({ path: names.slice(0, index + 1).join("\\") })} aria-current={index === names.length - 1 ? "page" : undefined}>{name}</button></li>
              ))}
            </ol>
            <button type="button" className="file-crumbs-edit" onClick={() => setPathEdit(keyDisplay(key))} aria-label="Type a key path">
              <PencilLine size={14} aria-hidden="true" />
            </button>
          </nav>
        )}
      </div>
      {problem && (
        <div className="error-banner" role="alert">
          <CircleAlert size={16} aria-hidden="true" />
          <span>{problem.message}{(() => { const help = errorHelp(problem.code); return help && help !== problem.message ? ` ${help}` : ""; })()}</span>
        </div>
      )}
      {notice && <p className="field-help tools-notice" role="status">{notice}</p>}
      {!shown ? (
        !problem && <div className="empty-state compact"><LoaderCircle size={22} className="spin" aria-hidden="true" /><strong>Opening {keyDisplay(key)}…</strong></div>
      ) : (
        <>
          {shown.truncated && <p className="field-help tools-notice">This key has more subkeys or values than one listing carries; some aren’t shown.</p>}
          {shown.modified_at && <p className="tools-status tools-notice">Changed {formatDateTime(shown.modified_at)}</p>}
          <div className="table-scroll">
            <table className="data-table registry-table" aria-label="Subkeys">
              <thead>
                <tr>
                  <th scope="col">Key</th>
                  {canManage && <th scope="col"><span className="sr-only">Actions</span></th>}
                </tr>
              </thead>
              <tbody>
                {subkeys.slice(0, MAX_ROWS).map((subkey) => (
                  <tr key={subkey.name}>
                    <td>
                      <button type="button" className="link-button registry-key" onClick={() => go({ path: childPath(key.path, subkey.name) })}>
                        <Folder size={15} aria-hidden="true" />{subkey.name}
                      </button>
                    </td>
                    {canManage && (
                      <td className="row-actions">
                        {names.length > 0 && <button type="button" className="link-button" onClick={(event) => open(event, { kind: "delete-key", subkey })}>Delete</button>}
                      </td>
                    )}
                  </tr>
                ))}
              </tbody>
            </table>
            {!subkeys.length && <p className="table-message field-help">{needle ? "No subkeys match." : "No subkeys."}</p>}
            {subkeys.length > MAX_ROWS && <p className="table-message field-help">Showing {MAX_ROWS.toLocaleString()} of {subkeys.length.toLocaleString()}; filter to find the rest.</p>}
          </div>
          <div className="table-scroll">
            <table className="data-table registry-table" aria-label="Values">
              <thead>
                <tr>
                  <th scope="col">Name</th>
                  <th scope="col">Type</th>
                  <th scope="col">Data</th>
                  {canManage && <th scope="col"><span className="sr-only">Actions</span></th>}
                </tr>
              </thead>
              <tbody>
                {!hasDefault && !needle && names.length > 0 && (
                  <tr>
                    <td><strong>(Default)</strong></td>
                    <td>REG_SZ</td>
                    <td className="registry-data muted">(value not set)</td>
                    {canManage && <td className="row-actions"><button type="button" className="link-button" onClick={(event) => open(event, { kind: "edit-value", value: { name: "", type: "string", data: "", size: 0 } })}>Edit</button></td>}
                  </tr>
                )}
                {values.slice(0, MAX_ROWS).map((value) => (
                  <tr key={value.name}>
                    <td><strong>{valueLabel(value.name)}</strong></td>
                    <td>{typeName(value)}</td>
                    <td className="registry-data" title={value.truncated ? `${value.size.toLocaleString()} bytes; cut short here` : undefined}>{formatData(value)}</td>
                    {canManage && (
                      <td className="row-actions">
                        {value.type !== "other" && <button type="button" className="link-button" onClick={(event) => open(event, { kind: "edit-value", value })}>Edit</button>}
                        {value.name !== "" && <button type="button" className="link-button" onClick={(event) => open(event, { kind: "rename-value", value })}>Rename</button>}
                        <button type="button" className="link-button" onClick={(event) => open(event, { kind: "delete-value", value })}>Delete</button>
                      </td>
                    )}
                  </tr>
                ))}
              </tbody>
            </table>
            {!values.length && hasDefault && <p className="table-message field-help">No values match.</p>}
            {values.length > MAX_ROWS && <p className="table-message field-help">Showing {MAX_ROWS.toLocaleString()} of {values.length.toLocaleString()}; filter to find the rest.</p>}
          </div>
        </>
      )}
      {dialog?.kind === "new-key" && shown && (
        <NewKeyDialog agent={agent} parent={key} taken={shown.subkeys.map((subkey) => subkey.name)} returnFocus={opener} onClose={() => setDialog(null)} onDone={done} />
      )}
      {dialog?.kind === "delete-key" && (
        <DeleteKeyDialog agent={agent} target={{ ...key, path: childPath(key.path, dialog.subkey.name) }} subkey={dialog.subkey} returnFocus={opener} onClose={() => setDialog(null)} onDone={done} />
      )}
      {dialog?.kind === "edit-value" && shown && (
        <ValueDialog agent={agent} target={key} value={dialog.value} taken={shown.values.map((value) => value.name)} returnFocus={opener} onClose={() => setDialog(null)} onDone={done} />
      )}
      {dialog?.kind === "rename-value" && shown && (
        <RenameValueDialog agent={agent} target={key} value={dialog.value} taken={shown.values.map((value) => value.name)} returnFocus={opener} onClose={() => setDialog(null)} onDone={done} />
      )}
      {dialog?.kind === "delete-value" && (
        <DeleteValueDialog agent={agent} target={key} value={dialog.value} returnFocus={opener} onClose={() => setDialog(null)} onDone={done} />
      )}
    </section>
  );
}

type DialogProps = {
  agent: Agent;
  returnFocus: RefObject<HTMLElement | null>;
  onClose: () => void;
  onDone: (notice: string) => void;
};

function RegistryDialog({ title, busy, onClose, returnFocus, children }: {
  title: string;
  busy: boolean;
  onClose: () => void;
  returnFocus: RefObject<HTMLElement | null>;
  children: (id: string) => ReactNode;
}) {
  const id = useId();
  return (
    <ModalDialog className="settings-modal file-dialog" labelledBy={`${id}-title`} onClose={busy ? () => {} : onClose} returnFocus={returnFocus}>
      <button type="button" className="modal-close" onClick={onClose} aria-label="Close" disabled={busy}><X size={19} /></button>
      <h2 id={`${id}-title`}>{title}</h2>
      {children(id)}
    </ModalDialog>
  );
}

// Runs one change, keeping the dialog open with why it failed.
function useChange(onDone: (notice: string) => void) {
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const run = async (change: () => Promise<unknown>, notice: string, fallback: string) => {
    setBusy(true);
    setError(null);
    try {
      await change();
      onDone(notice);
    } catch (failure) {
      if (!(failure instanceof AuthenticationRequired)) setError(errorText(failure, fallback));
      setBusy(false);
    }
  };
  return { busy, error, setError, run };
}

function Actions({ busy, error, onClose, label, danger, disabled, icon }: {
  busy: boolean;
  error: string | null;
  onClose: () => void;
  label: string;
  danger?: boolean;
  disabled?: boolean;
  icon: ReactNode;
}) {
  return (
    <>
      {error && <p role="alert" className="installer-error">{error}</p>}
      <div className="connection-reason-actions">
        <button type="button" className="secondary-button" onClick={onClose} disabled={busy}>Cancel</button>
        <button className={danger ? "danger-button" : "primary-button"} disabled={busy || disabled}>
          {busy ? <LoaderCircle size={16} className="spin" /> : icon} {label}
        </button>
      </div>
    </>
  );
}

function NewKeyDialog({ agent, parent, taken, returnFocus, onClose, onDone }: DialogProps & { parent: RegistryKey; taken: string[] }) {
  const { authorizedFetch } = useWorkspace();
  const [name, setName] = useState(() => freshName("New Key", taken));
  const { busy, error, setError, run } = useChange(onDone);
  const submit = (event: FormEvent) => {
    event.preventDefault();
    const problem = keyNameProblem(name);
    if (problem) return setError(problem);
    const key = { ...parent, path: childPath(parent.path, name) };
    void run(() => createRegistryKey(authorizedFetch, agent.id, key), `Created ${keyDisplay(key)}.`, "The key couldn’t be created.");
  };
  return (
    <RegistryDialog title="New key" busy={busy} onClose={onClose} returnFocus={returnFocus}>
      {(id) => (
        <form onSubmit={submit}>
          <p className="field-help">Under {keyDisplay(parent)}</p>
          <label htmlFor={`${id}-name`}>Name<input id={`${id}-name`} value={name} onChange={(event) => setName(event.target.value)} spellCheck={false} autoComplete="off" disabled={busy} /></label>
          <Actions busy={busy} error={error} onClose={onClose} label="Create" icon={<FolderPlus size={16} />} />
        </form>
      )}
    </RegistryDialog>
  );
}

function DeleteKeyDialog({ agent, target, subkey, returnFocus, onClose, onDone }: DialogProps & { target: RegistryKey; subkey: RegistrySubkey }) {
  const { authorizedFetch } = useWorkspace();
  const { busy, error, run } = useChange(onDone);
  const recursive = subkey.has_subkeys ?? false;
  const [sure, setSure] = useState(!recursive);
  const submit = (event: FormEvent) => {
    event.preventDefault();
    void run(() => deleteRegistryKey(authorizedFetch, agent.id, target, recursive), `Deleted ${keyDisplay(target)}.`, "The key couldn’t be deleted.");
  };
  return (
    <RegistryDialog title={`Delete ${subkey.name}?`} busy={busy} onClose={onClose} returnFocus={returnFocus}>
      {(id) => (
        <form onSubmit={submit}>
          <p><code className="registry-path">{keyDisplay(target)}</code></p>
          <p>
            {recursive
              ? "This deletes the key, every key under it and all their values, permanently. Windows or programs that read them may stop working."
              : "This deletes the key and its values, permanently. Windows or programs that read them may stop working."}
          </p>
          {recursive && (
            <label className="checkbox-row" htmlFor={`${id}-sure`}>
              <input id={`${id}-sure`} type="checkbox" checked={sure} onChange={(event) => setSure(event.target.checked)} disabled={busy} />
              Delete its subkeys too
            </label>
          )}
          <Actions busy={busy} error={error} onClose={onClose} label={recursive ? "Delete key and subkeys" : "Delete key"} danger disabled={!sure} icon={<Trash2 size={16} />} />
        </form>
      )}
    </RegistryDialog>
  );
}

// Creates a value, or edits one: a value too long for the listing is read
// whole first.
function ValueDialog({ agent, target, value, taken, returnFocus, onClose, onDone }: DialogProps & { target: RegistryKey; value: RegistryValue | null; taken: string[] }) {
  const { authorizedFetch } = useWorkspace();
  const creating = value === null;
  const [whole, setWhole] = useState<RegistryValue | null>(value && !value.truncated ? value : null);
  const [readError, setReadError] = useState<string | null>(null);
  const [name, setName] = useState(() => value?.name ?? freshName("New Value", taken));
  const [type, setType] = useState<Exclude<RegistryType, "other">>(value && value.type !== "other" ? value.type : "string");
  const [base, setBase] = useState<NumberBase>("hex");
  const [text, setText] = useState(() => (value && !value.truncated ? draftOf(value, "hex") : ""));
  const { busy, error, setError, run } = useChange(onDone);

  useEffect(() => {
    if (!value?.truncated) return;
    let current = true;
    getRegistryValue(authorizedFetch, agent.id, target, value.name).then(
      (read) => {
        if (!current) return;
        setWhole(read);
        setText(draftOf(read, "hex"));
      },
      (failure: unknown) => {
        if (current && !(failure instanceof AuthenticationRequired)) setReadError(errorText(failure, "The value couldn’t be read."));
      },
    );
    return () => {
      current = false;
    };
  }, [authorizedFetch, agent.id, target, value]);

  const tooLarge = whole?.truncated ?? false;
  const numeric = type === "dword" || type === "qword";
  const submit = (event: FormEvent) => {
    event.preventDefault();
    if (creating && taken.some((each) => each.toLowerCase() === name.toLowerCase())) return setError(`${valueLabel(name)} already exists.`);
    const parsed = dataFromDraft(type, text, base);
    if ("problem" in parsed) return setError(parsed.problem);
    void run(
      () => setRegistryValue(authorizedFetch, agent.id, target, name, parsed.data, creating),
      `${creating ? "Created" : "Saved"} ${valueLabel(name)}.`,
      "The value couldn’t be saved.",
    );
  };
  const ready = creating || (whole !== null && !tooLarge);
  return (
    <RegistryDialog title={creating ? "New value" : `Edit ${valueLabel(name)}`} busy={busy} onClose={onClose} returnFocus={returnFocus}>
      {(id) => (
        <form onSubmit={submit} className="registry-editor">
          <p className="field-help">In {keyDisplay(target)}</p>
          {creating && (
            <>
              <label htmlFor={`${id}-name`}>Name<input id={`${id}-name`} value={name} onChange={(event) => setName(event.target.value)} spellCheck={false} autoComplete="off" disabled={busy} /></label>
            </>
          )}
          <label htmlFor={`${id}-type`}>
            Type
            <select
            id={`${id}-type`}
            value={type}
            disabled={busy || !ready}
            onChange={(event) => {
              const next = event.target.value as Exclude<RegistryType, "other">;
              const wasNumeric = numeric;
              setType(next);
              if (wasNumeric !== (next === "dword" || next === "qword")) setText("");
            }}
          >
            {WRITABLE_TYPES.map((each) => <option key={each} value={each}>{TYPE_NAMES[each]}</option>)}
            </select>
          </label>
          {!ready ? (
            readError ? <p role="alert" className="installer-error">{readError}</p>
              : tooLarge ? <p className="installer-error">This value holds {whole?.size.toLocaleString()} bytes, more than the registry tool edits. Use Regedit or a script.</p>
              : <p className="field-help"><LoaderCircle size={14} className="spin" aria-hidden="true" /> Reading the whole value…</p>
          ) : (
            <>
              <div className="registry-field">
              <label htmlFor={`${id}-data`}>{type === "multi_string" ? "Strings, one per line" : type === "binary" ? "Bytes, in hexadecimal" : "Data"}</label>
              {numeric ? (
                <div className="registry-number">
                  <input id={`${id}-data`} value={text} onChange={(event) => setText(event.target.value)} spellCheck={false} autoComplete="off" inputMode={base === "hex" ? "text" : "numeric"} disabled={busy} />
                  <div className="segmented" role="group" aria-label="Base">
                    {(["hex", "decimal"] as NumberBase[]).map((each) => (
                      <button key={each} type="button" aria-pressed={base === each} disabled={busy} onClick={() => { setText(rebase(text, base, each)); setBase(each); }}>{each === "hex" ? "Hexadecimal" : "Decimal"}</button>
                    ))}
                  </div>
                </div>
              ) : type === "string" || type === "expand_string" ? (
                <input id={`${id}-data`} value={text} onChange={(event) => setText(event.target.value)} spellCheck={false} autoComplete="off" disabled={busy} />
              ) : (
                <textarea id={`${id}-data`} rows={8} value={text} onChange={(event) => setText(event.target.value)} spellCheck={false} disabled={busy} className={type === "binary" ? "mono-detail" : undefined} />
              )}
              </div>
              {type === "expand_string" && <p className="field-help">Programs that read it expand %VARIABLES% such as %SystemRoot%.</p>}
              {type === "multi_string" && <p className="field-help">Empty lines are left out, as Regedit does.</p>}
            </>
          )}
          <Actions busy={busy} error={error} onClose={onClose} label={creating ? "Create" : "Save"} disabled={!ready} icon={<Save size={16} />} />
        </form>
      )}
    </RegistryDialog>
  );
}

function RenameValueDialog({ agent, target, value, taken, returnFocus, onClose, onDone }: DialogProps & { target: RegistryKey; value: RegistryValue; taken: string[] }) {
  const { authorizedFetch } = useWorkspace();
  const [name, setName] = useState(value.name);
  const { busy, error, setError, run } = useChange(onDone);
  const submit = (event: FormEvent) => {
    event.preventDefault();
    if (!name) return setError("Enter a name. The default value has no name and can’t be renamed to.");
    if (taken.some((each) => each !== value.name && each.toLowerCase() === name.toLowerCase())) return setError(`${name} already exists.`);
    void run(() => renameRegistryValue(authorizedFetch, agent.id, target, value.name, name), `Renamed ${value.name} to ${name}.`, "The value couldn’t be renamed.");
  };
  return (
    <RegistryDialog title={`Rename ${value.name}`} busy={busy} onClose={onClose} returnFocus={returnFocus}>
      {(id) => (
        <form onSubmit={submit}>
          <label htmlFor={`${id}-name`}>New name<input id={`${id}-name`} value={name} onChange={(event) => setName(event.target.value)} spellCheck={false} autoComplete="off" disabled={busy} /></label>
          <Actions busy={busy} error={error} onClose={onClose} label="Rename" disabled={name === value.name} icon={<PencilLine size={16} />} />
        </form>
      )}
    </RegistryDialog>
  );
}

function DeleteValueDialog({ agent, target, value, returnFocus, onClose, onDone }: DialogProps & { target: RegistryKey; value: RegistryValue }) {
  const { authorizedFetch } = useWorkspace();
  const { busy, error, run } = useChange(onDone);
  const submit = (event: FormEvent) => {
    event.preventDefault();
    void run(() => deleteRegistryValue(authorizedFetch, agent.id, target, value.name), `Deleted ${valueLabel(value.name)}.`, "The value couldn’t be deleted.");
  };
  return (
    <RegistryDialog title={`Delete ${valueLabel(value.name)}?`} busy={busy} onClose={onClose} returnFocus={returnFocus}>
      {() => (
        <form onSubmit={submit}>
          <p>This deletes the value from <code className="registry-path">{keyDisplay(target)}</code>, permanently. Windows or programs that read it may stop working.</p>
          <p className="field-help">{typeName(value)} · {formatData(value)}</p>
          <Actions busy={busy} error={error} onClose={onClose} label="Delete value" danger icon={<Trash2 size={16} />} />
        </form>
      )}
    </RegistryDialog>
  );
}
