import { ChevronDown, ListFilter, LoaderCircle, X } from "lucide-react";
import { type FormEvent, type RefObject, useId, useState } from "react";
import { AuthenticationRequired, errorText } from "../../lib/http";
import { ModalDialog } from "../../lib/modal-dialog";
import { usePopover } from "../../lib/use-popover";
import type { Agent } from "../agents/types";
import { useWorkspace } from "../workspace/workspace-context";
import { FieldInput } from "./field-values";
import { setOnDevices } from "./fields-api";
import {
  type Draft,
  type FieldDefinition,
  type FieldFilter,
  type FieldOp,
  OP_LABELS,
  appliesToDevices,
  draftOf,
  draftValue,
  fieldFilterLabel,
  filterOps,
  opNeedsValue,
} from "./model";

// Shows the devices whose custom field matches: is a value, contains one,
// has a value or has none.
export function FieldFilterMenu({ fields, filter, onChange }: {
  fields: readonly FieldDefinition[];
  filter: FieldFilter | null;
  onChange: (filter: FieldFilter | null) => void;
}) {
  const { open, setOpen, container, trigger } = usePopover();
  const label = fieldFilterLabel(filter, fields);
  return (
    <div className="popover-anchor" ref={container}>
      <button ref={trigger} type="button" className="company-filter" onClick={() => setOpen(!open)} aria-expanded={open} aria-controls={open ? "field-filter-menu" : undefined} aria-label={`Custom field: ${label}`}>
        <ListFilter size={15} aria-hidden="true" /><span>{label}</span><ChevronDown size={14} aria-hidden="true" />
      </button>
      {open && <FieldFilterForm fields={fields} filter={filter} onChange={(next) => { onChange(next); setOpen(false); }} />}
    </div>
  );
}

function FieldFilterForm({ fields, filter, onChange }: {
  fields: readonly FieldDefinition[];
  filter: FieldFilter | null;
  onChange: (filter: FieldFilter | null) => void;
}) {
  const shown = fields.filter(appliesToDevices);
  const [key, setKey] = useState(filter?.key ?? shown[0]?.key ?? "");
  const field = shown.find((candidate) => candidate.key === key);
  const ops = field ? filterOps(field.type) : [];
  const [op, setOp] = useState<FieldOp>(filter?.op ?? ops[0] ?? "not_empty");
  const chosenOp = ops.includes(op) ? op : ops[0] ?? "not_empty";
  const [value, setValue] = useState(filter?.value ?? "");
  const needsValue = opNeedsValue(chosenOp);
  const ready = field !== undefined && (!needsValue || value.trim() !== "");

  const submit = (event: FormEvent) => {
    event.preventDefault();
    if (ready) onChange({ key, op: chosenOp, value: needsValue ? value.trim() : "" });
  };

  return (
    <form id="field-filter-menu" className="popover menu company-filter-menu field-filter-menu" aria-label="Show devices by custom field" onSubmit={submit}>
      <label>Field
        <select value={key} onChange={(event) => setKey(event.target.value)}>
          {shown.map((candidate) => <option key={candidate.key} value={candidate.key}>{candidate.label}</option>)}
        </select>
      </label>
      <label>Shows devices whose field
        <select value={chosenOp} onChange={(event) => setOp(event.target.value as FieldOp)}>
          {ops.map((candidate) => <option key={candidate} value={candidate}>{OP_LABELS[candidate]}</option>)}
        </select>
      </label>
      {needsValue && field && (
        field.type === "dropdown" || field.type === "multi_select" || field.type === "checkbox" ? (
          <label>Value
            <select value={value} onChange={(event) => setValue(event.target.value)}>
              <option value="">Choose…</option>
              {(field.type === "checkbox" ? ["true", "false"] : field.options).map((option) => (
                <option key={option} value={option}>{field.type === "checkbox" ? (option === "true" ? "Yes" : "No") : option}</option>
              ))}
            </select>
          </label>
        ) : (
          <label>Value<input type={field.type === "number" ? "number" : field.type === "date" ? "date" : "text"} step="any" value={value} onChange={(event) => setValue(event.target.value)} /></label>
        )
      )}
      <div className="notes-actions">
        {filter && <button type="button" className="link-button" onClick={() => onChange(null)}>Clear field filter</button>}
        <button className="primary-button" disabled={!ready}>Show devices</button>
      </div>
    </form>
  );
}

// Sets one custom field on the chosen devices, or clears it.
export function SetFieldDialog({ agents, fields, returnFocus, onClose }: {
  agents: Agent[];
  fields: readonly FieldDefinition[];
  returnFocus: RefObject<HTMLElement | null>;
  onClose: () => void;
}) {
  const { authorizedFetch } = useWorkspace();
  const titleId = useId();
  const inputId = useId();
  const shown = fields.filter(appliesToDevices);
  const [fieldId, setFieldId] = useState(shown[0]?.id ?? "");
  const field = shown.find((candidate) => candidate.id === fieldId);
  const [draft, setDraft] = useState<Draft>(() => (field ? draftOf(field, null) : ""));
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [done, setDone] = useState<number | null>(null);
  const checked = field ? draftValue(field, draft) : null;
  const problem = checked && "problem" in checked ? checked.problem : null;
  const clearing = checked !== null && "value" in checked && checked.value === null;

  const choose = (id: string) => {
    setFieldId(id);
    const next = shown.find((candidate) => candidate.id === id);
    setDraft(next ? draftOf(next, null) : "");
    setError(null);
  };

  const submit = async (event: FormEvent) => {
    event.preventDefault();
    if (!field || !checked || !("value" in checked)) return;
    setBusy(true);
    setError(null);
    try {
      const { changed } = await setOnDevices(authorizedFetch, field.id, agents.map((agent) => agent.id), checked.value);
      setDone(changed);
    } catch (failure) {
      if (!(failure instanceof AuthenticationRequired)) setError(errorText(failure, "The field could not be set on the devices."));
    } finally {
      setBusy(false);
    }
  };

  const count = `${agents.length} ${agents.length === 1 ? "device" : "devices"}`;
  return (
    <ModalDialog className="settings-modal" labelledBy={titleId} onClose={onClose} returnFocus={returnFocus}>
      <button type="button" className="modal-close" onClick={onClose} aria-label="Close"><X size={19} /></button>
      <h2 id={titleId}>Set a field on {count}</h2>
      {shown.length === 0 ? (
        <p>There are no device fields yet. Someone with the custom_fields.manage permission can define them on the Custom fields page.</p>
      ) : done !== null ? (
        <>
          <p role="status">{clearing ? "Cleared" : "Set"} “{field?.label}” on {done} {done === 1 ? "device" : "devices"}.</p>
          <div className="connection-reason-actions"><button type="button" className="primary-button" onClick={onClose}>Done</button></div>
        </>
      ) : (
        <form className="form-stack" onSubmit={(event) => void submit(event)}>
          <label>Field
            <select value={fieldId} disabled={busy} onChange={(event) => choose(event.target.value)}>
              {shown.map((candidate) => <option key={candidate.id} value={candidate.id}>{candidate.label}</option>)}
            </select>
          </label>
          {field && (
            <div className="form-stack-field">
              <label htmlFor={inputId}>Value</label>
              <FieldInput field={field} draft={draft} onChange={setDraft} disabled={busy} id={inputId} />
              <small className="field-help">{clearing ? `Saving clears the field on the ${count}${field.applies_to === "both" ? ", which then show their company’s value" : ""}.` : `Replaces each device’s own value.`}</small>
            </div>
          )}
          {(problem || error) && <p role="alert" className="installer-error">{problem ?? error}</p>}
          <div className="connection-reason-actions">
            <button type="button" className="secondary-button" onClick={onClose}>Cancel</button>
            <button className="primary-button" disabled={busy || !field || problem !== null}>{busy && <LoaderCircle size={16} className="spin" />}{clearing ? "Clear field" : "Set field"}</button>
          </div>
        </form>
      )}
    </ModalDialog>
  );
}
