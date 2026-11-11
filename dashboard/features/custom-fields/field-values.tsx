import { CircleAlert, Eye, EyeOff, LoaderCircle, Pencil, X } from "lucide-react";
import { type FormEvent, useCallback, useId, useState } from "react";
import { Link } from "react-router";
import { formatDateTime, formatRelative } from "../../lib/format";
import { AuthenticationRequired, errorText } from "../../lib/http";
import { useResource } from "../../lib/use-resource";
import { useWorkspace } from "../workspace/workspace-context";
import { type FieldOwner, fetchValues, revealValue, saveValue } from "./fields-api";
import { type Draft, type FieldDefinition, type FieldValue, authorText, displayValue, draftOf, draftValue, isWebAddress } from "./model";

// The input a field's type takes, for a draft of its value.
export function FieldInput({ field, draft, onChange, disabled, id }: {
  field: FieldDefinition;
  draft: Draft;
  onChange: (draft: Draft) => void;
  disabled: boolean;
  id: string;
}) {
  const text = typeof draft === "string" ? draft : "";
  switch (field.type) {
    case "multiline":
      return <textarea id={id} rows={4} value={text} disabled={disabled} onChange={(event) => onChange(event.target.value)} />;
    case "number":
      return <input id={id} type="number" step="any" inputMode="decimal" value={text} disabled={disabled} onChange={(event) => onChange(event.target.value)} />;
    case "date":
      return <input id={id} type="date" value={text} disabled={disabled} onChange={(event) => onChange(event.target.value)} />;
    case "url":
      return <input id={id} type="url" placeholder="https://" value={text} disabled={disabled} onChange={(event) => onChange(event.target.value)} />;
    case "secret":
      return <input id={id} type="password" autoComplete="new-password" placeholder="A new value" value={text} disabled={disabled} onChange={(event) => onChange(event.target.value)} />;
    case "checkbox":
      return (
        <select id={id} value={draft === "" ? "" : draft ? "yes" : "no"} disabled={disabled} onChange={(event) => onChange(event.target.value === "" ? "" : event.target.value === "yes")}>
          <option value="">No value</option>
          <option value="yes">Yes</option>
          <option value="no">No</option>
        </select>
      );
    case "dropdown":
      return (
        <select id={id} value={text} disabled={disabled} onChange={(event) => onChange(event.target.value)}>
          <option value="">No value</option>
          {/* A value from before its option was renamed stays choosable. */}
          {text && !field.options.includes(text) && <option value={text}>{text}</option>}
          {field.options.map((option) => <option key={option} value={option}>{option}</option>)}
        </select>
      );
    case "multi_select": {
      const chosen = Array.isArray(draft) ? draft : [];
      return (
        <fieldset id={id} className="field-options" disabled={disabled}>
          <legend className="sr-only">{field.label}</legend>
          {field.options.map((option) => (
            <label key={option} className="checkbox-row">
              <input type="checkbox" checked={chosen.includes(option)} onChange={(event) => onChange(event.target.checked ? [...chosen, option] : chosen.filter((candidate) => candidate !== option))} />
              <span>{option}</span>
            </label>
          ))}
        </fieldset>
      );
    }
    default:
      return <input id={id} value={text} disabled={disabled} onChange={(event) => onChange(event.target.value)} />;
  }
}

function Changed({ entry }: { entry: FieldValue }) {
  if (entry.updated_at === null) return null;
  const by = authorText(entry.updated_by);
  return (
    <span className="notes-meta">
      {by ? `Changed by ${by} ` : "Changed "}
      <time dateTime={new Date(entry.updated_at).toISOString()} title={formatDateTime(entry.updated_at)}>{formatRelative(entry.updated_at)}</time>
    </span>
  );
}

function Shown({ entry, revealed }: { entry: FieldValue; revealed: string | null }) {
  const { field } = entry;
  if (!entry.set) return <span className="field-help">No value</span>;
  if (field.type === "secret") return <code className="field-secret">{revealed ?? displayValue(field, null, true)}</code>;
  const text = displayValue(field, entry.value, entry.set);
  if (field.type === "url" && typeof entry.value === "string" && isWebAddress(entry.value)) {
    return <a href={entry.value} target="_blank" rel="noreferrer noopener">{text}</a>;
  }
  if (field.type === "multiline") return <p className="notes-text">{text}</p>;
  return <span>{text}</span>;
}

// One field's row: its value and where it came from, edited in place.
function FieldRow({ owner, entry, canEdit, canReveal, onSaved }: {
  owner: FieldOwner;
  entry: FieldValue;
  canEdit: boolean;
  canReveal: boolean;
  onSaved: (entry: FieldValue) => void;
}) {
  const { authorizedFetch } = useWorkspace();
  const inputId = useId();
  const { field } = entry;
  // A device's own value is edited; an inherited one starts the draft.
  const [draft, setDraft] = useState<Draft | null>(null);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [revealed, setRevealed] = useState<string | null>(null);
  const checked = draft === null ? null : draftValue(field, draft);
  const problem = checked && "problem" in checked ? checked.problem : null;

  const save = async (value: FieldValue["value"]) => {
    setBusy(true);
    setError(null);
    try {
      const saved = await saveValue(authorizedFetch, owner, field.id, value);
      onSaved(saved);
      setDraft(null);
      setRevealed(null);
    } catch (failure) {
      if (!(failure instanceof AuthenticationRequired)) setError(errorText(failure, "The value could not be saved."));
    } finally {
      setBusy(false);
    }
  };

  const submit = (event: FormEvent) => {
    event.preventDefault();
    if (checked && "value" in checked) void save(checked.value);
  };

  const reveal = async () => {
    if (revealed !== null) {
      setRevealed(null);
      return;
    }
    setError(null);
    try {
      setRevealed((await revealValue(authorizedFetch, owner, field.id)).value);
    } catch (failure) {
      if (!(failure instanceof AuthenticationRequired)) setError(errorText(failure, "The value could not be revealed."));
    }
  };

  // What clearing takes away: the device's own value, or the company's.
  const ownValue = entry.set && (owner.kind === "company" || entry.source === "device");
  return (
    <div className="custom-field-row">
      <dt>
        <label htmlFor={draft !== null ? inputId : undefined}>{field.label}</label>
        {field.description && <small>{field.description}</small>}
      </dt>
      <dd>
        {draft !== null ? (
          <form className="custom-field-editor" onSubmit={submit}>
            <FieldInput field={field} draft={draft} onChange={setDraft} disabled={busy} id={inputId} />
            <div className="notes-actions">
              <button type="button" className="secondary-button" disabled={busy} onClick={() => { setDraft(null); setError(null); }}>Cancel</button>
              <button className="primary-button" disabled={busy || problem !== null}>{busy && <LoaderCircle size={16} className="spin" />}Save</button>
            </div>
            {(problem || error) && <p role="alert" className="installer-error">{problem ?? error}</p>}
          </form>
        ) : (
          <>
            <div className="custom-field-value">
              <Shown entry={entry} revealed={revealed} />
              {field.type === "secret" && entry.set && canReveal && (
                <button type="button" className="link-button" onClick={() => void reveal()} aria-label={revealed === null ? `Reveal ${field.label}` : `Hide ${field.label}`}>
                  {revealed === null ? <><Eye size={14} aria-hidden="true" />Reveal</> : <><EyeOff size={14} aria-hidden="true" />Hide</>}
                </button>
              )}
              {canEdit && (
                <button type="button" className="link-button" onClick={() => { setDraft(field.type === "secret" ? "" : draftOf(field, entry.value)); setError(null); }} aria-label={`Edit ${field.label}`}>
                  <Pencil size={14} aria-hidden="true" />{entry.set ? (field.type === "secret" ? "Replace" : "Edit") : "Set"}
                </button>
              )}
              {canEdit && ownValue && (
                <button type="button" className="link-button" disabled={busy} onClick={() => void save(null)} aria-label={`Clear ${field.label}`}>
                  <X size={14} aria-hidden="true" />{owner.kind === "device" && entry.field.applies_to === "both" ? "Use the company’s" : "Clear"}
                </button>
              )}
            </div>
            <div className="custom-field-meta">
              {entry.source === "company" && entry.inherited_from && (
                <span className="notes-meta">From the company <Link to={`/company?id=${encodeURIComponent(entry.inherited_from.id)}`}>{entry.inherited_from.name}</Link></span>
              )}
              <Changed entry={entry} />
            </div>
            {error && <p role="alert" className="installer-error">{error}</p>}
          </>
        )}
      </dd>
    </div>
  );
}

// The "Fields" section of a device's or company's page.
export function FieldValues({ owner, canEdit }: { owner: FieldOwner; canEdit: boolean }) {
  const { authorizedFetch, can } = useWorkspace();
  const titleId = useId();
  const load = useCallback(() => fetchValues(authorizedFetch, owner), [authorizedFetch, owner]);
  const { data, error, reload, setData } = useResource(load, "The fields could not be loaded.");
  const canManage = can("custom_fields.manage");
  const replace = useCallback((saved: FieldValue) => {
    setData((current) => current.map((entry) => (entry.field.id === saved.field.id ? saved : entry)));
  }, [setData]);

  if (data && data.length === 0 && !canManage) return null;
  return (
    <section className="device-notes custom-fields" aria-labelledby={titleId}>
      <div className="device-alerts-heading">
        <h3 id={titleId}>Fields</h3>
        {data && <button type="button" className="link-button" onClick={reload}>Refresh</button>}
      </div>
      {!data ? (
        error
          ? <p className="form-error" role="alert"><CircleAlert size={16} aria-hidden="true" />{error}</p>
          : <p className="field-help" role="status"><LoaderCircle size={16} className="spin" /> Loading fields…</p>
      ) : data.length === 0 ? (
        <p className="field-help">No custom fields yet. <Link to="/custom-fields">Define some</Link> for what you track about each {owner.kind}.</p>
      ) : (
        <dl className="custom-field-list">
          {data.map((entry) => (
            <FieldRow key={entry.field.id} owner={owner} entry={entry} canEdit={canEdit} canReveal={can("custom_fields.reveal")} onSaved={replace} />
          ))}
        </dl>
      )}
    </section>
  );
}
