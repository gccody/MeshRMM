import { ArrowDown, ArrowUp, CircleAlert, ListPlus, LoaderCircle, Pencil, Plus, Trash2, X } from "lucide-react";
import { type FormEvent, type RefObject, useCallback, useId, useRef, useState } from "react";
import { AuthenticationRequired, errorText } from "../../lib/http";
import { ModalDialog } from "../../lib/modal-dialog";
import { useResource } from "../../lib/use-resource";
import { GroupingTabs } from "../companies/area-tabs";
import { HeaderActions } from "../workspace/header-actions";
import { useWorkspace } from "../workspace/workspace-context";
import { createDefinition, deleteDefinition, fetchDefinitions, reorderDefinitions, updateDefinition } from "./fields-api";
import {
  APPLIES_TO,
  type AppliesTo,
  FIELD_TYPES,
  type FieldDefinition,
  type FieldType,
  MAX_DESCRIPTION_CHARS,
  MAX_KEY_CHARS,
  MAX_LABEL_CHARS,
  SCRIPT_ACCESS,
  type ScriptAccess,
  fieldVariable,
  hasOptions,
  keyProblem,
  parseOptions,
  suggestedKey,
  typeLabel,
} from "./model";

const APPLIES_LABEL: Record<AppliesTo, string> = { devices: "Devices", companies: "Companies", both: "Devices and companies" };
const ACCESS_LABEL: Record<ScriptAccess, string> = { none: "None", read: "Read", read_write: "Read and write" };

// Makes a field, or changes one. A field's key and type stay once made.
function FieldDialog({ field, returnFocus, onClose, onSaved }: {
  field: FieldDefinition | null;
  returnFocus: RefObject<HTMLElement | null>;
  onClose: () => void;
  onSaved: (field: FieldDefinition) => void;
}) {
  const { authorizedFetch } = useWorkspace();
  const titleId = useId();
  const [label, setLabel] = useState(field?.label ?? "");
  const [key, setKey] = useState(field?.key ?? "");
  // The key follows the label until someone types one.
  const [keyTouched, setKeyTouched] = useState(field !== null);
  const [description, setDescription] = useState(field?.description ?? "");
  const [type, setType] = useState<FieldType>(field?.type ?? "text");
  const [optionsText, setOptionsText] = useState(field?.options.join("\n") ?? "");
  const [appliesTo, setAppliesTo] = useState<AppliesTo>(field?.applies_to ?? "devices");
  const [access, setAccess] = useState<ScriptAccess>(field?.script_access ?? "none");
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const shownKey = keyTouched ? key : suggestedKey(label);
  const options = parseOptions(optionsText);
  const problem = !label.trim()
    ? "Give the field a label."
    : (!field && keyProblem(shownKey)) || (hasOptions(type) && (options.problem ?? (options.options.length === 0 ? "List the options, one per line." : null))) || null;

  const submit = async (event: FormEvent) => {
    event.preventDefault();
    if (problem) return;
    setBusy(true);
    setError(null);
    const input = {
      label: label.trim(),
      description: description.trim(),
      options: hasOptions(type) ? options.options : [],
      applies_to: appliesTo,
      script_access: access,
    };
    try {
      const saved = field
        ? await updateDefinition(authorizedFetch, field.id, input)
        : await createDefinition(authorizedFetch, { ...input, key: shownKey, type });
      onSaved(saved);
      onClose();
    } catch (failure) {
      if (!(failure instanceof AuthenticationRequired)) setError(errorText(failure, "The field could not be saved."));
      setBusy(false);
    }
  };

  // Changing where a field applies drops the values left without a place.
  const narrowing = field !== null && field.applies_to !== appliesTo && appliesTo !== "both";
  const removed = field && hasOptions(field.type) ? field.options.filter((option) => !options.options.includes(option)) : [];
  return (
    <ModalDialog className="settings-modal" labelledBy={titleId} onClose={onClose} returnFocus={returnFocus}>
      <button type="button" className="modal-close" onClick={onClose} aria-label="Close"><X size={19} /></button>
      <h2 id={titleId}>{field ? `Edit ${field.label}` : "New custom field"}</h2>
      <form className="form-stack" onSubmit={(event) => void submit(event)}>
        <label>Label<input required maxLength={MAX_LABEL_CHARS} value={label} disabled={busy} onChange={(event) => setLabel(event.target.value)} placeholder="Contract tier" /></label>
        <label>Key
          <input
            required
            maxLength={MAX_KEY_CHARS}
            value={shownKey}
            disabled={busy || field !== null}
            onChange={(event) => { setKeyTouched(true); setKey(event.target.value); }}
            spellCheck={false}
          />
          <small className="field-help">{field ? "A field’s key doesn’t change." : "Scripts and the API know the field by it; it can’t change later."} Scripts read it as <code>{fieldVariable(shownKey || "key")}</code>.</small>
        </label>
        <label>Description<textarea rows={2} maxLength={MAX_DESCRIPTION_CHARS} value={description} disabled={busy} onChange={(event) => setDescription(event.target.value)} placeholder="Optional" /></label>
        <label>Type
          <select value={type} disabled={busy || field !== null} onChange={(event) => setType(event.target.value as FieldType)}>
            {FIELD_TYPES.map((entry) => <option key={entry.type} value={entry.type}>{entry.label}</option>)}
          </select>
          {type === "secret" && <small className="field-help">Encrypted on the server and hidden on the website. Revealing a value needs the custom_fields.reveal permission and is recorded in the audit log.</small>}
        </label>
        {hasOptions(type) && (
          <label>Options, one per line
            <textarea rows={5} value={optionsText} disabled={busy} onChange={(event) => setOptionsText(event.target.value)} />
            {removed.length > 0 && <small className="field-help">Removing an option takes it out of the values that hold it.</small>}
          </label>
        )}
        <label>Applies to
          <select value={appliesTo} disabled={busy} onChange={(event) => setAppliesTo(event.target.value as AppliesTo)}>
            {APPLIES_TO.map((entry) => <option key={entry.value} value={entry.value}>{entry.label}</option>)}
          </select>
          {appliesTo === "both" && <small className="field-help">A device without its own value shows its company’s.</small>}
          {narrowing && <small className="field-help">Saving removes the values {appliesTo === "devices" ? "companies" : "devices"} hold.</small>}
        </label>
        <fieldset className="choice-list" disabled={busy}>
          <legend>Scripts</legend>
          {SCRIPT_ACCESS.map((entry) => (
            <label key={entry.value} className="checkbox-row">
              <input type="radio" name="script-access" checked={access === entry.value} onChange={() => setAccess(entry.value)} />
              <span><strong>{entry.label}</strong>{entry.help}</span>
            </label>
          ))}
        </fieldset>
        {(error || (problem && label)) && <p role="alert" className="installer-error">{error ?? problem}</p>}
        <div className="connection-reason-actions">
          <button type="button" className="secondary-button" onClick={onClose}>Cancel</button>
          <button className="primary-button" disabled={busy || problem !== null}>{busy && <LoaderCircle size={16} className="spin" />}{field ? "Save" : "Create field"}</button>
        </div>
      </form>
    </ModalDialog>
  );
}

// `/custom-fields`: the fields devices and companies have, for those who
// define them.
export function CustomFieldsPage() {
  const { authorizedFetch, can } = useWorkspace();
  const canManage = can("custom_fields.manage");
  const load = useCallback(() => fetchDefinitions(authorizedFetch), [authorizedFetch]);
  const { data, error, setData } = useResource(load, "The custom fields could not be loaded.");
  const [dialog, setDialog] = useState<{ field: FieldDefinition | null } | null>(null);
  const opener = useRef<HTMLElement | null>(null);
  const [busyId, setBusyId] = useState<string | null>(null);
  const [actionError, setActionError] = useState<string | null>(null);

  const open = (field: FieldDefinition | null, button: HTMLElement) => {
    opener.current = button;
    setDialog({ field });
  };

  const saved = (field: FieldDefinition) =>
    setData((current) => (current.some((entry) => entry.id === field.id) ? current.map((entry) => (entry.id === field.id ? field : entry)) : [...current, field]));

  const remove = async (field: FieldDefinition) => {
    if (!window.confirm(`Delete the field “${field.label}”? Every device’s and company’s value of it is deleted too, and scripts that use ${fieldVariable(field.key)} no longer get it.`)) return;
    setBusyId(field.id);
    setActionError(null);
    try {
      await deleteDefinition(authorizedFetch, field.id);
      setData((current) => current.filter((entry) => entry.id !== field.id));
    } catch (failure) {
      if (!(failure instanceof AuthenticationRequired)) setActionError(errorText(failure, "The field could not be deleted."));
    } finally {
      setBusyId(null);
    }
  };

  const move = async (index: number, by: -1 | 1) => {
    if (!data) return;
    const ids = data.map((field) => field.id);
    const [moved] = ids.splice(index, 1);
    ids.splice(index + by, 0, moved);
    setBusyId(moved);
    setActionError(null);
    try {
      const fields = await reorderDefinitions(authorizedFetch, ids);
      setData(() => fields);
    } catch (failure) {
      if (!(failure instanceof AuthenticationRequired)) setActionError(errorText(failure, "The fields could not be reordered."));
    } finally {
      setBusyId(null);
    }
  };

  return (
    <>
      {actionError && (
        <div className="error-banner" role="alert">
          <CircleAlert size={17} aria-hidden="true" /><span>{actionError}</span>
          <button onClick={() => setActionError(null)} aria-label="Dismiss"><X size={16} /></button>
        </div>
      )}
      <HeaderActions>
        {canManage && <button type="button" className="primary-button" onClick={(event) => open(null, event.currentTarget)} aria-haspopup="dialog"><Plus size={16} /> New field</button>}
      </HeaderActions>
      <GroupingTabs current="custom-fields" />
      <section className="agent-panel">
        {!data ? (
          error
            ? <p className="form-error" role="alert"><CircleAlert size={16} aria-hidden="true" />{error}</p>
            : <p role="status" className="field-help"><LoaderCircle size={16} className="spin" /> Loading custom fields…</p>
        ) : data.length === 0 ? (
          <div className="empty-state">
            <ListPlus size={22} aria-hidden="true" />
            <strong>No custom fields yet</strong>
            <span>Track what matters to you on devices and companies, such as a contract tier, a backup vendor or the date of the last disaster recovery test, and let scripts read and fill them in.</span>
          </div>
        ) : (
          <table className="data-table" aria-label="Custom fields">
            <thead><tr><th scope="col">Field</th><th scope="col">Type</th><th scope="col">Applies to</th><th scope="col">Scripts</th><th scope="col"><span className="sr-only">Actions</span></th></tr></thead>
            <tbody>
              {data.map((field, index) => (
                <tr key={field.id}>
                  <td>
                    <strong>{field.label}</strong>
                    <span><code>{field.key}</code>{field.description ? ` · ${field.description}` : ""}</span>
                  </td>
                  <td>{typeLabel(field.type)}{field.options.length > 0 && <span>{field.options.join(", ")}</span>}</td>
                  <td>{APPLIES_LABEL[field.applies_to]}</td>
                  <td>{ACCESS_LABEL[field.script_access]}</td>
                  <td className="row-actions">
                    {canManage && <>
                      <button type="button" className="close-session-button" disabled={busyId !== null || index === 0} onClick={() => void move(index, -1)} aria-label={`Move ${field.label} up`} title="Move up"><ArrowUp size={16} /></button>
                      <button type="button" className="close-session-button" disabled={busyId !== null || index === data.length - 1} onClick={() => void move(index, 1)} aria-label={`Move ${field.label} down`} title="Move down"><ArrowDown size={16} /></button>
                      <button type="button" className="close-session-button" onClick={(event) => open(field, event.currentTarget)} aria-haspopup="dialog" aria-label={`Edit ${field.label}`} title="Edit field"><Pencil size={16} /></button>
                      <button type="button" className="agent-delete-button" disabled={busyId === field.id} onClick={() => void remove(field)} aria-label={`Delete ${field.label}`} title="Delete field">{busyId === field.id ? <LoaderCircle size={16} className="spin" /> : <Trash2 size={16} />}</button>
                    </>}
                  </td>
                </tr>
              ))}
            </tbody>
          </table>
        )}
      </section>
      {dialog && <FieldDialog field={dialog.field} returnFocus={opener} onClose={() => setDialog(null)} onSaved={saved} />}
    </>
  );
}
