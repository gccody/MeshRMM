import { FolderPlus, LoaderCircle, MoveRight, PencilLine, Trash2, X } from "lucide-react";
import { type FormEvent, type ReactNode, type RefObject, useEffect, useId, useRef, useState } from "react";
import { AuthenticationRequired, errorText } from "../../lib/http";
import { ModalDialog } from "../../lib/modal-dialog";
import type { DevicePlatform } from "../agents/types";
import { type FileAccount, type FileBatch, type FileEntry, type FileFailure, accountLabel, nameProblem, stemLength, trashName } from "./model";

type Base = {
  platform: DevicePlatform;
  onClose: () => void;
  returnFocus?: RefObject<HTMLElement | null>;
};

// A dialog that runs one action, shows its error, and closes when it's done.
function ActionDialog({ id, title, children, onClose, returnFocus, busy }: {
  id: string;
  title: string;
  children: ReactNode;
  onClose: () => void;
  returnFocus?: RefObject<HTMLElement | null>;
  busy: boolean;
}) {
  return (
    <ModalDialog className="settings-modal file-dialog" labelledBy={id} onClose={busy ? () => {} : onClose} returnFocus={returnFocus}>
      <button type="button" className="modal-close" onClick={onClose} aria-label="Close" disabled={busy}><X size={19} /></button>
      <h2 id={id}>{title}</h2>
      {children}
    </ModalDialog>
  );
}

function useAction(onClose: () => void) {
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const run = async (action: () => Promise<boolean | void>, fallback: string) => {
    setBusy(true);
    setError(null);
    try {
      if ((await action()) !== false) onClose();
    } catch (failure) {
      if (!(failure instanceof AuthenticationRequired)) setError(errorText(failure, fallback));
    } finally {
      setBusy(false);
    }
  };
  return { busy, error, run };
}

// A name for a new folder, or a new name for an entry.
function NameDialog({ platform, title, label, initial, selectLength, submitLabel, icon, onSubmit, onClose, returnFocus }: Base & {
  title: string;
  label: string;
  initial: string;
  selectLength: number;
  submitLabel: string;
  icon: ReactNode;
  onSubmit: (name: string) => Promise<void>;
}) {
  const id = useId();
  const [name, setName] = useState(initial);
  const input = useRef<HTMLInputElement>(null);
  const { busy, error, run } = useAction(onClose);
  const problem = nameProblem(platform, name);
  useEffect(() => {
    input.current?.focus();
    input.current?.setSelectionRange(0, selectLength);
  }, [selectLength]);
  const submit = (event: FormEvent) => {
    event.preventDefault();
    if (problem) return;
    void run(() => onSubmit(name), "It couldn’t be saved.");
  };
  return (
    <ActionDialog id={`${id}-title`} title={title} onClose={onClose} returnFocus={returnFocus} busy={busy}>
      <form onSubmit={submit}>
        <fieldset className="script-editor-fields" disabled={busy}>
          <label htmlFor={`${id}-name`}>{label}<input ref={input} id={`${id}-name`} value={name} onChange={(event) => setName(event.target.value)} aria-invalid={Boolean(problem && name)} spellCheck={false} autoComplete="off" /></label>
          {problem && name && <small className="field-help">{problem}</small>}
        </fieldset>
        {error && <p role="alert" className="installer-error">{error}</p>}
        <div className="connection-reason-actions">
          <button type="button" className="secondary-button" onClick={onClose} disabled={busy}>Cancel</button>
          <button className="primary-button" disabled={busy || Boolean(problem) || name === initial && initial !== ""}>
            {busy ? <LoaderCircle size={16} className="spin" /> : icon} {submitLabel}
          </button>
        </div>
      </form>
    </ActionDialog>
  );
}

export function NewFolderDialog(props: Base & { onCreate: (name: string) => Promise<void> }) {
  return (
    <NameDialog {...props} title="New folder" label="Name" initial="" selectLength={0} submitLabel="Create folder" icon={<FolderPlus size={16} />} onSubmit={props.onCreate} />
  );
}

export function RenameDialog(props: Base & { entry: FileEntry; onRename: (name: string) => Promise<void> }) {
  return (
    <NameDialog
      {...props}
      title={`Rename “${props.entry.name}”`}
      label="New name"
      initial={props.entry.name}
      selectLength={stemLength(props.entry.name, props.entry.kind)}
      submitLabel="Rename"
      icon={<PencilLine size={16} />}
      onSubmit={props.onRename}
    />
  );
}

// What a move or delete couldn't do, entry by entry.
function Failures({ failures }: { failures: FileFailure[] }) {
  if (!failures.length) return null;
  return (
    <ul className="file-failures" role="alert">
      {failures.slice(0, 20).map((failure) => <li key={failure.path}><strong>{failure.path}</strong>{failure.error.message}</li>)}
      {failures.length > 20 && <li>and {failures.length - 20} more</li>}
    </ul>
  );
}

// An entry with where it is, as the page chose it.
export type Chosen = FileEntry & { path: string };

const describe = (entries: FileEntry[]) => (entries.length === 1 ? `“${entries[0].name}”` : `${entries.length} items`);

// The entries of `entries` that `batch` couldn't do, to try again.
const leftOver = (entries: Chosen[], batch: FileBatch) =>
  entries.filter((entry) => batch.failed.some((failure) => failure.path === entry.path));

export function MoveDialog({ entries: chosen, folder, suggestions, onMove, onClose, returnFocus, platform }: Base & {
  entries: Chosen[];
  folder: string;
  // Folders to offer: drives, places, and those in view.
  suggestions: string[];
  onMove: (paths: string[], destination: string) => Promise<FileBatch>;
}) {
  const id = useId();
  const [destination, setDestination] = useState(folder);
  const [entries, setEntries] = useState(chosen);
  const [failures, setFailures] = useState<FileFailure[]>([]);
  const { busy, error, run } = useAction(onClose);
  const submit = (event: FormEvent) => {
    event.preventDefault();
    void run(async () => {
      const batch = await onMove(entries.map((entry) => entry.path), destination.trim());
      setFailures(batch.failed);
      setEntries(leftOver(entries, batch));
      return batch.failed.length === 0;
    }, "They couldn’t be moved.");
  };
  return (
    <ActionDialog id={`${id}-title`} title={`Move ${describe(entries)}`} onClose={onClose} returnFocus={returnFocus} busy={busy}>
      <p>Into another folder on the device. Nothing there is replaced; an entry whose name is taken stays where it is.</p>
      <form onSubmit={submit}>
        <fieldset className="script-editor-fields" disabled={busy}>
          <label htmlFor={`${id}-to`}>Destination folder<input id={`${id}-to`} value={destination} onChange={(event) => setDestination(event.target.value)} list={`${id}-places`} spellCheck={false} autoComplete="off" placeholder={platform === "windows" ? "C:\\Users\\Public" : "/home"} /></label>
          <datalist id={`${id}-places`}>{suggestions.map((path) => <option key={path} value={path} />)}</datalist>
        </fieldset>
        {error && <p role="alert" className="installer-error">{error}</p>}
        <Failures failures={failures} />
        <div className="connection-reason-actions">
          <button type="button" className="secondary-button" onClick={onClose} disabled={busy}>{failures.length ? "Close" : "Cancel"}</button>
          <button className="primary-button" disabled={busy || !destination.trim() || destination.trim() === folder}>
            {busy ? <LoaderCircle size={16} className="spin" /> : <MoveRight size={16} />} Move
          </button>
        </div>
      </form>
    </ActionDialog>
  );
}

export function DeleteDialog({ entries: chosen, account, platform, onDelete, onClose, returnFocus }: Base & {
  entries: Chosen[];
  account: FileAccount;
  onDelete: (paths: string[], trash: boolean) => Promise<FileBatch>;
}) {
  const id = useId();
  const [entries, setEntries] = useState(chosen);
  // Only the signed-in user has a Recycle Bin or Trash to put things in.
  const canTrash = account === "user";
  const [trash, setTrash] = useState(canTrash);
  const [failures, setFailures] = useState<FileFailure[]>([]);
  const { busy, error, run } = useAction(onClose);
  const bin = trashName(platform);
  const folders = entries.filter((entry) => entry.kind === "directory" && !entry.link).length;
  const unavailable = failures.some((failure) => failure.error.code === "trash_unavailable");
  const submit = (event: FormEvent) => {
    event.preventDefault();
    void run(async () => {
      const batch = await onDelete(entries.map((entry) => entry.path), trash);
      setFailures(batch.failed);
      setEntries(leftOver(entries, batch));
      return batch.failed.length === 0;
    }, "They couldn’t be deleted.");
  };
  return (
    <ActionDialog id={`${id}-title`} title={`Delete ${describe(entries)}?`} onClose={onClose} returnFocus={returnFocus} busy={busy}>
      {entries.length > 1 && (
        <ul className="file-delete-list">
          {entries.slice(0, 8).map((entry) => <li key={entry.name}>{entry.name}</li>)}
          {entries.length > 8 && <li>and {entries.length - 8} more</li>}
        </ul>
      )}
      {folders > 0 && <p>{folders === 1 && entries.length === 1 ? "The folder" : "Folders"} and everything in {folders === 1 && entries.length === 1 ? "it" : "them"} will be deleted.</p>}
      <form onSubmit={submit}>
        {canTrash ? (
          <fieldset className="run-as-choice file-delete-choice" disabled={busy}>
            <legend className="sr-only">How to delete</legend>
            <label>
              <input type="radio" name={`${id}-how`} checked={trash} onChange={() => setTrash(true)} />
              <span><strong>Move to the {bin}</strong>The signed-in user can get {entries.length === 1 ? "it" : "them"} back from there.</span>
            </label>
            <label>
              <input type="radio" name={`${id}-how`} checked={!trash} onChange={() => setTrash(false)} />
              <span><strong>Delete permanently</strong>{entries.length === 1 ? "It" : "They"} can’t be recovered.</span>
            </label>
          </fieldset>
        ) : (
          <p className="file-delete-warning">{accountLabel("system", platform)} has no {bin}, so this deletes {entries.length === 1 ? "it" : "them"} permanently. To use the {bin}, browse as the signed-in user.</p>
        )}
        {error && <p role="alert" className="installer-error">{error}</p>}
        <Failures failures={failures} />
        {unavailable && trash && <p className="field-help">Some entries can’t go to the {bin}; choose Delete permanently to delete them.</p>}
        <div className="connection-reason-actions">
          <button type="button" className="secondary-button" onClick={onClose} disabled={busy}>{failures.length ? "Close" : "Cancel"}</button>
          <button className="danger-button" disabled={busy}>
            {busy ? <LoaderCircle size={16} className="spin" /> : <Trash2 size={16} />} {trash && canTrash ? `Move to ${bin}` : "Delete permanently"}
          </button>
        </div>
      </form>
    </ActionDialog>
  );
}
