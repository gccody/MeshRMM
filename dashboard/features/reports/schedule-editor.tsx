import { LoaderCircle, X } from "lucide-react";
import { type FormEvent, type RefObject, useCallback, useId, useMemo, useState } from "react";
import { AuthenticationRequired, errorText } from "../../lib/http";
import { ModalDialog } from "../../lib/modal-dialog";
import { useResource } from "../../lib/use-resource";
import { useCompanies, useTags } from "../agents/use-devices";
import { timeZones } from "../automation/model";
import { useWorkspace } from "../workspace/workspace-context";
import {
  FREQUENCY_LABELS,
  type Frequency,
  MAX_DAY_OF_MONTH,
  PERIOD_LABELS,
  type RollingPeriod,
  type Schedule,
  type ScheduleDraft,
  type TemplateId,
  type Templates,
  WEEKDAY_NAMES,
  scheduleDraft,
  scheduleDraftProblem,
  scheduleInput,
} from "./model";
import { fetchRecipients, saveSchedule } from "./reports-api";
import { browserZone } from "./run-dialog";
import { ScopeFields, ZoneSelect } from "./scope-fields";

const FREQUENCIES = Object.keys(FREQUENCY_LABELS) as Frequency[];
const ROLLING: RollingPeriod[] = ["last_7_days", "last_30_days", "previous_month"];

// Creates a schedule, or edits one. Saving makes the user its owner: its
// reports cover what they may see from then on.
export function ScheduleEditor({ schedule, templates, returnFocus, onClose, onSaved }: {
  schedule: Schedule | null;
  templates: Templates;
  returnFocus: RefObject<HTMLElement | null>;
  onClose: () => void;
  onSaved: (schedule: Schedule) => void;
}) {
  const { authorizedFetch, inventory } = useWorkspace();
  const companies = useCompanies(inventory.devices);
  const tags = useTags(inventory.devices);
  const titleId = useId();
  const zones = useMemo(() => timeZones(Intl.supportedValuesOf("timeZone")), []);
  const loadRecipients = useCallback(() => fetchRecipients(authorizedFetch), [authorizedFetch]);
  const recipients = useResource(loadRecipients, "The users could not be loaded.");
  const [draft, setDraft] = useState<ScheduleDraft>(() => scheduleDraft(schedule, browserZone(), templates.default_stale_days));
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const chosen = templates.templates.find((candidate) => candidate.id === draft.template);
  const problem = scheduleDraftProblem(draft, zones, templates.max_stale_days);
  const update = (change: Partial<ScheduleDraft>) => setDraft((current) => ({ ...current, ...change }));
  const toggleUser = (id: string, checked: boolean) => update({ userIds: checked ? [...draft.userIds, id] : draft.userIds.filter((candidate) => candidate !== id) });

  const submit = async (event: FormEvent) => {
    event.preventDefault();
    if (problem) return;
    setBusy(true);
    setError(null);
    try {
      onSaved(await saveSchedule(authorizedFetch, schedule?.id ?? null, scheduleInput(draft)));
    } catch (failure) {
      if (!(failure instanceof AuthenticationRequired)) setError(errorText(failure, "The schedule could not be saved."));
      setBusy(false);
    }
  };

  return (
    <ModalDialog className="settings-modal role-modal" labelledBy={titleId} onClose={onClose} returnFocus={returnFocus}>
      <button type="button" className="modal-close" onClick={onClose} aria-label="Close"><X size={19} /></button>
      <h2 id={titleId}>{schedule ? `Edit ${schedule.name}` : "New report schedule"}</h2>
      <p>The server makes the report on time, keeps it here, and emails it to the recipients. It runs with your access: saving makes you its owner{schedule && !schedule.owned_by_you ? ` in place of ${schedule.owner}` : ""}.</p>
      <form onSubmit={(event) => void submit(event)}>
        <fieldset className="script-editor-fields" disabled={busy}>
          <label htmlFor="schedule-name">Name<input id="schedule-name" required maxLength={120} value={draft.name} onChange={(event) => update({ name: event.target.value })} placeholder="Weekly summary for Acme" /></label>
          <label htmlFor="schedule-template">Report
            <select id="schedule-template" value={draft.template} onChange={(event) => update({ template: event.target.value as TemplateId })}>
              {templates.templates.map((candidate) => <option key={candidate.id} value={candidate.id}>{candidate.title}</option>)}
            </select>
          </label>
          <ScopeFields name="schedule-target" draft={draft} companies={companies} tags={tags} onChange={update} />
          {chosen?.uses_period && (
            <label htmlFor="schedule-period">Covers
              <select id="schedule-period" value={draft.period} onChange={(event) => update({ period: event.target.value as RollingPeriod })}>
                {ROLLING.map((period) => <option key={period} value={period}>{PERIOD_LABELS[period]}</option>)}
              </select>
            </label>
          )}
          {chosen?.uses_stale_days && (
            <label htmlFor="schedule-stale">Count devices as not seen after
              <span className="input-with-unit">
                <input id="schedule-stale" type="number" inputMode="numeric" min={1} max={templates.max_stale_days} step={1} required value={draft.staleDays} onChange={(event) => update({ staleDays: event.target.value })} />
                <span aria-hidden="true">days</span>
              </span>
            </label>
          )}
          <label htmlFor="schedule-frequency">Runs
            <select id="schedule-frequency" value={draft.frequency} onChange={(event) => update({ frequency: event.target.value as Frequency })}>
              {FREQUENCIES.map((frequency) => <option key={frequency} value={frequency}>{FREQUENCY_LABELS[frequency]}</option>)}
            </select>
          </label>
          {draft.frequency === "weekly" && (
            <label htmlFor="schedule-weekday">On
              <select id="schedule-weekday" value={draft.weekday} onChange={(event) => update({ weekday: Number(event.target.value) })}>
                {WEEKDAY_NAMES.map((day, index) => <option key={day} value={index + 1}>{day}</option>)}
              </select>
            </label>
          )}
          {draft.frequency === "monthly" && (
            <label htmlFor="schedule-day">On day
              <input id="schedule-day" type="number" inputMode="numeric" min={1} max={MAX_DAY_OF_MONTH} step={1} required value={draft.dayOfMonth} onChange={(event) => update({ dayOfMonth: event.target.value })} />
            </label>
          )}
          <div className="script-editor-row">
            <label htmlFor="schedule-time">At<input id="schedule-time" type="time" required value={draft.time} onChange={(event) => update({ time: event.target.value.slice(0, 5) })} /></label>
            <ZoneSelect id="schedule-zone" zones={zones} value={draft.timeZone} onChange={(timeZone) => update({ timeZone })} />
          </div>
          <small className="field-help">The period is judged in this zone too: &ldquo;last 7 days&rdquo; is the 7 whole days before the day it runs.</small>

          <fieldset className="choice-list picker-list">
            <legend>Email to</legend>
            {!recipients.data
              ? <p className="field-help">{recipients.error ?? "Loading users…"}</p>
              : recipients.data.users.map((user) => (
                <label key={user.id}>
                  <input type="checkbox" checked={draft.userIds.includes(user.id)} onChange={(event) => toggleUser(user.id, event.target.checked)} />
                  <span><strong>{user.display_name}</strong>{user.email}</span>
                </label>
              ))}
          </fieldset>
          <label htmlFor="schedule-emails">Other addresses
            <textarea id="schedule-emails" rows={3} value={draft.emails} onChange={(event) => update({ emails: event.target.value })} placeholder="it-lead@customer.example" />
          </label>
          <small className="field-help">One per line. {templates.email_configured ? "Each recipient gets their own email, with the report attached when it isn't too large." : "Email isn't set up on this server yet, so nothing is sent until it is; the reports are still kept here."}</small>
          <label className="checkbox-row">
            <input type="checkbox" checked={draft.enabled} onChange={(event) => update({ enabled: event.target.checked })} />
            <span><strong>Turned on</strong>A schedule that&apos;s off makes nothing; Run now still works.</span>
          </label>
        </fieldset>
        {(error || (problem && draft.name.trim())) && <p role="alert" className="installer-error">{error ?? problem}</p>}
        <div className="connection-reason-actions">
          <button type="button" className="secondary-button" onClick={onClose}>Cancel</button>
          <button className="primary-button" disabled={busy || problem !== null}>{busy && <LoaderCircle size={16} className="spin" />} {schedule ? "Save schedule" : "Create schedule"}</button>
        </div>
      </form>
    </ModalDialog>
  );
}
