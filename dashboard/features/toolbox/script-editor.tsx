
import { LoaderCircle, X } from "lucide-react";
import { type FormEvent, type RefObject, useEffect, useId, useState } from "react";
import { AuthenticationRequired } from "../../lib/http";
import { ModalDialog } from "../../lib/modal-dialog";
import { useWorkspace } from "../workspace/workspace-context";
import {
  LANGUAGE_LABELS,
  MAX_SCRIPT_TIMEOUT_SECONDS,
  MIN_SCRIPT_TIMEOUT_SECONDS,
  type ScriptDraft,
  type ToolboxScript,
  emptyScriptDraft,
  scriptDraft,
  scriptDraftProblem,
} from "./model";
import type { Sharing } from "./access";
import { fetchScript, saveScript } from "./toolbox-api";

type Props = {
  /** The script to edit or view; `null` writes a new one. */
  script: ToolboxScript | null;
  /** Folders to suggest. */
  folders: string[];
  /** The folder a new script starts in. */
  initialFolder?: string;
  // Whether the user may share the script, must, or can't.
  sharing: Sharing;
  onClose: () => void;
  /** The saved script, or `null` when the user can no longer see it. */
  onSaved: (script: ToolboxScript | null, id: string | null) => void;
  returnFocus?: RefObject<HTMLElement | null>;
};

const PLACEHOLDERS: Record<ScriptDraft["language"], string> = {
  powershell: "Get-Service -Name Spooler | Restart-Service -PassThru",
  cmd: "@echo off\nipconfig /flushdns",
  shell: "dscacheutil -flushcache\nkillall -HUP mDNSResponder",
};

/** Writes a new script, or edits or views one. */
export function ScriptEditor({ script, folders, initialFolder = "", sharing, onClose, onSaved, returnFocus }: Props) {
  const { authorizedFetch } = useWorkspace();
  const [draft, setDraft] = useState<ScriptDraft>(() => (script ? scriptDraft(script) : { ...emptyScriptDraft(initialFolder), shared: sharing === "required" }));
  const [loading, setLoading] = useState(Boolean(script && script.body === undefined));
  const [saving, setSaving] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [touched, setTouched] = useState(false);
  const folderList = useId();
  const readOnly = Boolean(script && !script.can_edit);
  const problem = scriptDraftProblem(draft);

  useEffect(() => {
    if (!script || script.body !== undefined) return;
    let cancelled = false;
    fetchScript(authorizedFetch, script.id)
      .then((loaded) => { if (!cancelled) setDraft(scriptDraft(loaded)); })
      .catch((loadError: unknown) => {
        if (!cancelled && !(loadError instanceof AuthenticationRequired)) {
          setError(loadError instanceof Error ? loadError.message : "The script could not be loaded.");
        }
      })
      .finally(() => { if (!cancelled) setLoading(false); });
    return () => { cancelled = true; };
  }, [authorizedFetch, script]);

  const update = (change: Partial<ScriptDraft>) => setDraft((current) => ({ ...current, ...change }));

  const submit = async (event: FormEvent) => {
    event.preventDefault();
    setTouched(true);
    if (problem || readOnly) return;
    setSaving(true);
    setError(null);
    try {
      onSaved(await saveScript(authorizedFetch, script?.id ?? null, draft), script?.id ?? null);
    } catch (saveError) {
      if (!(saveError instanceof AuthenticationRequired)) {
        setError(saveError instanceof Error ? saveError.message : "The script could not be saved.");
      }
    } finally {
      setSaving(false);
    }
  };

  const title = !script ? "New script" : readOnly ? script.name : "Edit script";
  return (
    <ModalDialog className="settings-modal toolbox-modal" labelledBy="script-editor-title" onClose={onClose} returnFocus={returnFocus}>
      <button type="button" className="modal-close" onClick={onClose} aria-label="Close"><X size={19} /></button>
      <h2 id="script-editor-title">{title}</h2>
      <p>{readOnly
        ? "You can run this script, but only its owner can change it, or someone who manages shared scripts while it is shared."
        : "Scripts run on devices from this website, or from the toolbox in a remote session."}</p>
      <form onSubmit={(event) => void submit(event)}>
        <fieldset className="script-editor-fields" disabled={readOnly || loading || saving}>
          <div className="script-editor-row">
            <label htmlFor="script-name">Name<input id="script-name" value={draft.name} maxLength={200} onChange={(event) => update({ name: event.target.value })} required /></label>
            <label htmlFor="script-folder">Folder<input id="script-folder" value={draft.folder} list={folderList} placeholder="Top level" onChange={(event) => update({ folder: event.target.value })} /></label>
            <datalist id={folderList}>{folders.map((folder) => <option key={folder} value={folder} />)}</datalist>
          </div>
          <label htmlFor="script-description">Description (optional)<input id="script-description" value={draft.description} onChange={(event) => update({ description: event.target.value })} /></label>
          <div className="script-editor-row">
            <label htmlFor="script-language">Interpreter
              <select id="script-language" value={draft.language} onChange={(event) => update({ language: event.target.value as ScriptDraft["language"] })}>
                {(["powershell", "cmd", "shell"] as const).map((language) => <option key={language} value={language}>{LANGUAGE_LABELS[language]}</option>)}
              </select>
            </label>
            <label htmlFor="script-timeout">Stop after (seconds)<input id="script-timeout" type="number" inputMode="numeric" min={MIN_SCRIPT_TIMEOUT_SECONDS} max={MAX_SCRIPT_TIMEOUT_SECONDS} value={draft.timeoutSeconds} onChange={(event) => update({ timeoutSeconds: event.target.value })} /></label>
          </div>
          <label htmlFor="script-body">Script
            <textarea id="script-body" className="script-body" rows={14} spellCheck={false} autoCapitalize="off" autoCorrect="off" value={loading ? "Loading…" : draft.body} placeholder={PLACEHOLDERS[draft.language]} onChange={(event) => update({ body: event.target.value })} />
          </label>
          <label className="toolbox-share">
            <input type="checkbox" checked={draft.shared} disabled={sharing !== "optional"} onChange={(event) => update({ shared: event.target.checked })} />
            <span><strong>Share with the whole company</strong>Everyone in the company can see and run it. Otherwise only you can.</span>
          </label>
        </fieldset>
        {touched && problem && !readOnly && <p role="alert" className="installer-error">{problem}</p>}
        {error && <p role="alert" className="installer-error">{error}</p>}
        <div className="connection-reason-actions">
          <button type="button" className="secondary-button" onClick={onClose}>{readOnly ? "Close" : "Cancel"}</button>
          {!readOnly && <button className="primary-button" disabled={loading || saving}>{saving && <LoaderCircle size={16} className="spin" />} Save script</button>}
        </div>
      </form>
    </ModalDialog>
  );
}
