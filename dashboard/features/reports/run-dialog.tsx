import { LoaderCircle, X } from "lucide-react";
import { type FormEvent, type RefObject, useId, useMemo, useState } from "react";
import { AuthenticationRequired, errorText } from "../../lib/http";
import { ModalDialog } from "../../lib/modal-dialog";
import { useCompanies, useTags } from "../agents/use-devices";
import { canEverywhere } from "../auth/types";
import { currentZoneName, timeZones } from "../automation/model";
import { useWorkspace } from "../workspace/workspace-context";
import { PERIOD_LABELS, type PeriodKind, type Report, type RunDraft, type TemplateId, type Templates, runDraft, runDraftProblem, runInput } from "./model";
import { runReport } from "./reports-api";
import { ScopeFields, ZoneSelect } from "./scope-fields";

const PERIODS = Object.keys(PERIOD_LABELS) as PeriodKind[];

export const browserZone = () => currentZoneName(Intl.DateTimeFormat().resolvedOptions().timeZone || "UTC");

// Makes a report now: which one, on which devices, over which days.
export function RunDialog({ templates, template, returnFocus, onClose, onQueued }: {
  templates: Templates;
  template: TemplateId;
  returnFocus: RefObject<HTMLElement | null>;
  onClose: () => void;
  onQueued: (report: Report) => void;
}) {
  const { account, authorizedFetch, inventory } = useWorkspace();
  // Limited to some companies, a report covers chosen companies or tags.
  const everyDevice = canEverywhere(account, "reports.view");
  const companies = useCompanies(inventory.devices);
  const tags = useTags(inventory.devices);
  const titleId = useId();
  const zones = useMemo(() => timeZones(Intl.supportedValuesOf("timeZone")), []);
  const [draft, setDraft] = useState<RunDraft>(() => {
    const initial = runDraft(template, browserZone(), templates.default_stale_days);
    return everyDevice || initial.target !== "all" ? initial : { ...initial, target: "companies" };
  });
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const chosen = templates.templates.find((candidate) => candidate.id === draft.template);
  const problem = runDraftProblem(draft, templates, zones);
  const update = (change: Partial<RunDraft>) => setDraft((current) => ({ ...current, ...change }));

  const submit = async (event: FormEvent) => {
    event.preventDefault();
    if (problem) return;
    setBusy(true);
    setError(null);
    try {
      onQueued(await runReport(authorizedFetch, runInput(draft)));
    } catch (failure) {
      if (!(failure instanceof AuthenticationRequired)) setError(errorText(failure, "The report could not be started."));
      setBusy(false);
    }
  };

  return (
    <ModalDialog className="settings-modal role-modal" labelledBy={titleId} onClose={onClose} returnFocus={returnFocus}>
      <button type="button" className="modal-close" onClick={onClose} aria-label="Close"><X size={19} /></button>
      <h2 id={titleId}>Run a report</h2>
      <p>The server makes it in the background; it shows in the list when it&apos;s ready, and is kept for {templates.retention_days} days.</p>
      <form onSubmit={(event) => void submit(event)}>
        <fieldset className="script-editor-fields" disabled={busy}>
          <label htmlFor="report-template">Report
            <select id="report-template" value={draft.template} onChange={(event) => update({ template: event.target.value as TemplateId })}>
              {templates.templates.map((candidate) => <option key={candidate.id} value={candidate.id}>{candidate.title}</option>)}
            </select>
          </label>
          {chosen && <small className="field-help">{chosen.description}</small>}
          <ScopeFields name="report-target" draft={draft} companies={companies} tags={tags} onChange={update} everyDevice={everyDevice} />
          {chosen?.uses_period && (
            <label htmlFor="report-period">Period
              <select id="report-period" value={draft.period} onChange={(event) => update({ period: event.target.value as PeriodKind })}>
                {PERIODS.map((period) => <option key={period} value={period}>{PERIOD_LABELS[period]}</option>)}
              </select>
            </label>
          )}
          {chosen?.uses_period && draft.period === "custom" && (
            <div className="script-editor-row">
              <label htmlFor="report-from">First day<input id="report-from" type="date" required value={draft.from} onChange={(event) => update({ from: event.target.value })} /></label>
              <label htmlFor="report-to">Last day<input id="report-to" type="date" required value={draft.to} onChange={(event) => update({ to: event.target.value })} /></label>
            </div>
          )}
          <ZoneSelect id="report-zone" zones={zones} value={draft.timeZone} onChange={(timeZone) => update({ timeZone })} />
          <small className="field-help">{chosen?.uses_period ? "Days start and end at midnight in this zone, and the report shows times in it." : "The report shows times in this zone."}</small>
          {chosen?.uses_stale_days && (
            <label htmlFor="report-stale">Count devices as not seen after
              <span className="input-with-unit">
                <input id="report-stale" type="number" inputMode="numeric" min={1} max={templates.max_stale_days} step={1} required value={draft.staleDays} onChange={(event) => update({ staleDays: event.target.value })} />
                <span aria-hidden="true">days</span>
              </span>
            </label>
          )}
        </fieldset>
        {(error || problem) && <p role="alert" className="installer-error">{error ?? problem}</p>}
        <div className="connection-reason-actions">
          <button type="button" className="secondary-button" onClick={onClose}>Cancel</button>
          <button className="primary-button" disabled={busy || problem !== null}>{busy && <LoaderCircle size={16} className="spin" />} Run report</button>
        </div>
      </form>
    </ModalDialog>
  );
}
