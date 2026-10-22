import { CircleAlert, History, LoaderCircle, Pencil, TriangleAlert } from "lucide-react";
import { useCallback, useEffect, useId, useRef, useState } from "react";
import { formatDateTime, formatRelative } from "../../lib/format";
import { AuthenticationRequired, RequestError, errorText } from "../../lib/http";
import { useResource } from "../../lib/use-resource";
import type { Agent } from "../agents/types";
import { useWorkspace } from "../workspace/workspace-context";
import { MAX_NOTES_CHARS, type Notes, type RevisionText, charactersText, conflictText, editorName, notesLength, notesParts, notesProblem } from "./model";
import { fetchNotes, fetchRevision, saveNotes } from "./notes-api";

// Notes text: whitespace kept, web addresses as links, nothing as HTML.
function NotesText({ text }: { text: string }) {
  return (
    <p className="notes-text">
      {notesParts(text).map((part, index) => part.kind === "link"
        ? <a key={index} href={part.text} target="_blank" rel="noreferrer noopener">{part.text}</a>
        : <span key={index}>{part.text}</span>)}
    </p>
  );
}

function Edited({ name, at }: { name: string | null; at: number }) {
  return <>{editorName(name)} <time dateTime={new Date(at).toISOString()} title={formatDateTime(at)}>{formatRelative(at)}</time></>;
}

type Editing = { text: string; base: number };

// A device's notes on its page: who last edited them and when, an editor
// that refuses to overwrite a newer save silently, and the kept history.
export function DeviceNotes({ agent }: { agent: Agent }) {
  const { authorizedFetch, can } = useWorkspace();
  const canEdit = can("devices.edit");
  const titleId = useId();
  const load = useCallback(() => fetchNotes(authorizedFetch, agent.id), [authorizedFetch, agent.id]);
  const { data, error, reload, setData } = useResource(load, "The device’s notes could not be loaded.");
  const [editing, setEditing] = useState<Editing | null>(null);
  // Notes someone else saved while this user was editing.
  const [conflict, setConflict] = useState<Notes | null>(null);
  const [showTheirs, setShowTheirs] = useState(false);
  const [saving, setSaving] = useState(false);
  const [saveError, setSaveError] = useState<string | null>(null);
  const [viewing, setViewing] = useState<RevisionText | null>(null);
  const [historyError, setHistoryError] = useState<string | null>(null);
  const editor = useRef<HTMLTextAreaElement>(null);
  const liveRevision = agent.notes_revision ?? 0;

  const replace = useCallback((notes: Notes) => setData(() => notes), [setData]);

  // A newer save, as the event socket tells: shown at once when nobody is
  // editing, and as a warning to the user who is.
  useEffect(() => {
    if (!data || saving || liveRevision <= data.revision) return;
    if (!editing) {
      reload();
      return;
    }
    if (conflict && conflict.revision >= liveRevision) return;
    let cancelled = false;
    fetchNotes(authorizedFetch, agent.id).then((notes) => {
      if (cancelled) return;
      replace(notes);
      if (notes.revision > editing.base) setConflict(notes);
    }, () => {});
    return () => {
      cancelled = true;
    };
  }, [agent.id, authorizedFetch, conflict, data, editing, liveRevision, reload, replace, saving]);

  const startEditing = (text: string) => {
    if (!data) return;
    setEditing({ text, base: data.revision });
    setConflict(null);
    setShowTheirs(false);
    setSaveError(null);
    window.requestAnimationFrame(() => editor.current?.focus());
  };

  const stopEditing = () => {
    setEditing(null);
    setConflict(null);
    setShowTheirs(false);
    setSaveError(null);
  };

  // Saves over the revision the edit started from, or, once the user chose
  // to keep their text, over the newer one.
  const save = async (over: number) => {
    if (!editing) return;
    setSaving(true);
    setSaveError(null);
    try {
      const saved = await saveNotes(authorizedFetch, agent.id, editing.text, over);
      replace(saved);
      stopEditing();
    } catch (failure) {
      if (failure instanceof AuthenticationRequired) return;
      if (failure instanceof RequestError && failure.code === "notes_changed") {
        try {
          const current = await fetchNotes(authorizedFetch, agent.id);
          replace(current);
          setConflict(current);
        } catch (reloadFailure) {
          if (!(reloadFailure instanceof AuthenticationRequired)) setSaveError(errorText(reloadFailure, "The notes could not be loaded."));
        }
      } else {
        setSaveError(errorText(failure, "The notes could not be saved."));
      }
    } finally {
      setSaving(false);
    }
  };

  const view = async (revision: number) => {
    setHistoryError(null);
    try {
      setViewing(await fetchRevision(authorizedFetch, agent.id, revision));
    } catch (failure) {
      if (!(failure instanceof AuthenticationRequired)) setHistoryError(errorText(failure, "That version of the notes could not be loaded."));
    }
  };

  const problem = editing ? notesProblem(editing.text) : null;
  const length = editing ? notesLength(editing.text) : 0;
  const older = data?.history.slice(1) ?? [];

  return (
    <section className="device-notes" aria-labelledby={titleId}>
      <div className="device-alerts-heading">
        <h3 id={titleId}>Notes</h3>
        {data && canEdit && !editing && <button type="button" className="link-button" onClick={() => startEditing(data.body)}><Pencil size={14} aria-hidden="true" />{data.body ? "Edit" : "Add notes"}</button>}
      </div>
      {!data ? (
        error
          ? <p className="form-error" role="alert"><CircleAlert size={16} aria-hidden="true" />{error}</p>
          : <p className="field-help" role="status"><LoaderCircle size={16} className="spin" /> Loading notes…</p>
      ) : editing ? (
        <div className="notes-editor">
          {conflict && (
            <div className="notes-conflict" role="alert">
              <p><TriangleAlert size={16} aria-hidden="true" />{conflictText(conflict)}</p>
              <div className="notes-actions">
                <button type="button" className="link-button" onClick={() => setShowTheirs(!showTheirs)} aria-expanded={showTheirs}>{showTheirs ? "Hide their version" : "Show their version"}</button>
                <button type="button" className="secondary-button" disabled={saving || problem !== null} onClick={() => void save(conflict.revision)}>Save mine anyway</button>
                <button type="button" className="secondary-button" disabled={saving} onClick={stopEditing}>Discard mine</button>
              </div>
              {showTheirs && (conflict.body ? <NotesText text={conflict.body} /> : <p className="field-help">They cleared the notes.</p>)}
            </div>
          )}
          <label className="sr-only" htmlFor={`${titleId}-text`}>Notes</label>
          <textarea id={`${titleId}-text`} ref={editor} rows={8} value={editing.text} disabled={saving} onChange={(event) => setEditing({ ...editing, text: event.target.value })} placeholder="Where it is, who uses it, quirks to know about. Web addresses become links." />
          <div className="notes-actions">
            <small className={problem ? "notes-count over" : "notes-count"}>{length.toLocaleString()} / {MAX_NOTES_CHARS.toLocaleString()}</small>
            <button type="button" className="secondary-button" disabled={saving} onClick={stopEditing}>Cancel</button>
            {!conflict && <button type="button" className="primary-button" disabled={saving || problem !== null || editing.text === data.body} onClick={() => void save(editing.base)}>{saving && <LoaderCircle size={16} className="spin" />}Save notes</button>}
          </div>
          {(saveError || problem) && <p role="alert" className="installer-error">{saveError ?? problem}</p>}
        </div>
      ) : data.body ? (
        <NotesText text={data.body} />
      ) : (
        <p className="field-help">{canEdit ? "No notes yet. Add where the device is, who uses it, or anything the next person should know." : "No notes yet."}</p>
      )}
      {data && data.updated_at !== null && <p className="notes-meta">{data.body ? "Edited by" : "Cleared by"} <Edited name={data.updated_by} at={data.updated_at} /></p>}
      {older.length > 0 && (
        <details className="notes-history">
          <summary><History size={14} aria-hidden="true" />History ({older.length} earlier {older.length === 1 ? "version" : "versions"})</summary>
          {historyError && <p className="form-error" role="alert"><CircleAlert size={16} aria-hidden="true" />{historyError}</p>}
          <ul>
            {older.map((revision) => (
              <li key={revision.revision}>
                <span><Edited name={revision.updated_by} at={revision.updated_at} /> · {charactersText(revision.length)}</span>
                <button type="button" className="link-button" onClick={() => viewing?.revision === revision.revision ? setViewing(null) : void view(revision.revision)} aria-expanded={viewing?.revision === revision.revision}>
                  {viewing?.revision === revision.revision ? "Hide" : "View"}
                </button>
                {viewing?.revision === revision.revision && (
                  <div className="notes-version">
                    {viewing.body ? <NotesText text={viewing.body} /> : <p className="field-help">Empty.</p>}
                    {canEdit && !editing && <button type="button" className="secondary-button" onClick={() => { startEditing(viewing.body); setViewing(null); }}>Restore this version</button>}
                  </div>
                )}
              </li>
            ))}
          </ul>
          {canEdit && <p className="field-help">Restoring puts that version in the editor; saving it makes a new version.</p>}
        </details>
      )}
    </section>
  );
}
