import { LoaderCircle, X } from "lucide-react";
import { type FormEvent, type RefObject, useId, useState } from "react";
import { AuthenticationRequired, errorText } from "../../lib/http";
import { ModalDialog } from "../../lib/modal-dialog";
import { useNow } from "../../lib/use-now";
import type { Agent, MaintenanceAutomation } from "../agents/types";
import { useCompanies, useDevices, useTags } from "../agents/use-devices";
import { DevicePicker } from "../device-picker/device-picker";
import { TagTargetFields } from "../tags/tag-chip";
import type { TagMatch } from "../tags/model";
import { useWorkspace } from "../workspace/workspace-context";
import { endMaintenance, startMaintenance } from "./maintenance-api";
import {
  AUTOMATION_CHOICES,
  DURATION_CHOICES,
  MAX_REASON_CHARS,
  type MaintenanceTarget,
  type StartDraft,
  inMaintenance,
  localInput,
  maintenanceTitle,
  startBody,
  startDraft,
  startDraftProblem,
} from "./model";

const AUTOMATIONS = Object.keys(AUTOMATION_CHOICES) as MaintenanceAutomation[];

type TargetDraft = { target: MaintenanceTarget["target"]; deviceIds: string[]; companyIds: string[]; tagIds: string[]; tagMatch: TagMatch };

function targetOf(draft: TargetDraft): MaintenanceTarget {
  switch (draft.target) {
    case "devices": return { target: "devices", device_ids: draft.deviceIds };
    case "companies": return { target: "companies", company_ids: draft.companyIds };
    case "tags": return { target: "tags", tag_ids: draft.tagIds, tag_match: draft.tagMatch };
    case "all": return { target: "all" };
  }
}

function targetProblem(draft: TargetDraft) {
  if (draft.target === "devices" && !draft.deviceIds.length) return "Choose at least one device.";
  if (draft.target === "companies" && !draft.companyIds.length) return "Choose at least one company.";
  if (draft.target === "tags" && !draft.tagIds.length) return "Choose at least one tag.";
  return null;
}

// Puts devices in maintenance, now or later: the chosen `agents`, or with
// none the devices the dialog's target covers. Chosen devices already in
// maintenance can have it ended from here too.
export function StartMaintenanceDialog({ agents, returnFocus, onClose, onDone }: {
  agents: readonly Agent[] | null;
  returnFocus: RefObject<HTMLElement | null>;
  onClose: () => void;
  // After windows started or ended.
  onDone?: () => void;
}) {
  const { authorizedFetch, inventory } = useWorkspace();
  const companies = useCompanies(inventory.devices);
  const tags = useTags(inventory.devices);
  const fleet = useDevices(inventory.devices);
  const titleId = useId();
  const [draft, setDraft] = useState<StartDraft>(startDraft);
  const [target, setTarget] = useState<TargetDraft>({ target: "all", deviceIds: [], companyIds: [], tagIds: [], tagMatch: "any" });
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const now = useNow(15_000);
  const update = (change: Partial<StartDraft>) => setDraft((current) => ({ ...current, ...change }));
  const chosen = agents ? targetOf({ ...target, target: "devices", deviceIds: agents.map((agent) => agent.id) }) : targetOf(target);
  const problem = startDraftProblem(draft, now) ?? (agents ? null : targetProblem(target));
  const held = (agents ?? []).filter(inMaintenance);
  const title = agents ? maintenanceTitle(agents.map((agent) => agent.name)) : "Put devices in maintenance";

  const run = async (action: () => Promise<unknown>, fallback: string) => {
    setBusy(true);
    setError(null);
    try {
      await action();
      onDone?.();
      onClose();
    } catch (failure) {
      if (!(failure instanceof AuthenticationRequired)) setError(errorText(failure, fallback));
      setBusy(false);
    }
  };
  const submit = (event: FormEvent) => {
    event.preventDefault();
    if (problem) return;
    void run(() => startMaintenance(authorizedFetch, startBody(draft, chosen)), "The devices could not be put in maintenance.");
  };
  const endHeld = () => void run(
    () => endMaintenance(authorizedFetch, { target: "devices", device_ids: held.map((agent) => agent.id) }),
    "Maintenance could not be ended.",
  );

  return (
    <ModalDialog className="settings-modal role-modal" labelledBy={titleId} onClose={onClose} returnFocus={returnFocus}>
      <button type="button" className="modal-close" onClick={onClose} aria-label="Close"><X size={19} /></button>
      <h2 id={titleId}>{title}</h2>
      <p>While a device is in maintenance it raises no alerts and sends no notifications, and self-healing waits. Its open alerts stay as they are until it ends, when the device is judged again. People can still connect to it, open terminals and run jobs on it.</p>
      {held.length > 0 && (
        <div className="maintenance-held">
          <span>{held.length === agents?.length ? (held.length === 1 ? "It is" : "They are all") : `${held.length} of them ${held.length === 1 ? "is" : "are"}`} in maintenance already.</span>
          <button type="button" className="secondary-button" disabled={busy} onClick={endHeld}>End maintenance{held.length > 1 ? ` on ${held.length}` : ""}</button>
        </div>
      )}
      <form onSubmit={submit}>
        <fieldset className="script-editor-fields" disabled={busy}>
          <label htmlFor="maintenance-reason">Reason
            <input id="maintenance-reason" required maxLength={MAX_REASON_CHARS} value={draft.reason} onChange={(event) => update({ reason: event.target.value })} placeholder="Replacing the disk" />
          </label>
          {!agents && (
            <>
              <fieldset className="choice-list">
                <legend>Devices</legend>
                <label><input type="radio" name="maintenance-target" checked={target.target === "all"} onChange={() => setTarget({ ...target, target: "all" })} /><span><strong>Every device</strong>As they are now.</span></label>
                <label><input type="radio" name="maintenance-target" checked={target.target === "companies"} disabled={!companies.length} onChange={() => setTarget({ ...target, target: "companies" })} /><span><strong>Devices of chosen companies</strong>{companies.length ? "Choose them below." : "There are no companies yet."}</span></label>
                <label><input type="radio" name="maintenance-target" checked={target.target === "tags"} disabled={!tags.length} onChange={() => setTarget({ ...target, target: "tags" })} /><span><strong>Devices with chosen tags</strong>{tags.length ? "The devices that have them now." : "There are no tags yet."}</span></label>
                <label><input type="radio" name="maintenance-target" checked={target.target === "devices"} onChange={() => setTarget({ ...target, target: "devices" })} /><span><strong>Chosen devices</strong>{target.deviceIds.length ? `${target.deviceIds.length} chosen` : "Choose them below."}</span></label>
              </fieldset>
              {target.target === "companies" && (
                <fieldset className="choice-list picker-list">
                  <legend className="sr-only">Companies</legend>
                  {companies.map((company) => (
                    <label key={company.id}>
                      <input type="checkbox" checked={target.companyIds.includes(company.id)} onChange={(event) => setTarget({ ...target, companyIds: event.target.checked ? [...target.companyIds, company.id] : target.companyIds.filter((id) => id !== company.id) })} />
                      <strong>{company.name}</strong>
                    </label>
                  ))}
                </fieldset>
              )}
              {target.target === "tags" && <TagTargetFields tags={tags} chosen={target.tagIds} match={target.tagMatch} onChange={({ tagIds, tagMatch }) => setTarget({ ...target, ...(tagIds && { tagIds }), ...(tagMatch && { tagMatch }) })} />}
              {target.target === "devices" && <DevicePicker agents={fleet} chosen={target.deviceIds} onChange={(deviceIds) => setTarget({ ...target, deviceIds: [...deviceIds] })} language={null} />}
            </>
          )}
          <fieldset className="choice-list">
            <legend>Starts</legend>
            <label><input type="radio" name="maintenance-start" checked={!draft.later} onChange={() => update({ later: false })} /><span><strong>Now</strong>Its alerts are held back at once.</span></label>
            <label><input type="radio" name="maintenance-start" checked={draft.later} onChange={() => update({ later: true, startsAt: draft.startsAt || localInput(now + 60 * 60_000) })} /><span><strong>Later</strong>It shows as upcoming until then.</span></label>
          </fieldset>
          {draft.later && <label htmlFor="maintenance-starts-at">At<input id="maintenance-starts-at" type="datetime-local" required value={draft.startsAt} onChange={(event) => update({ startsAt: event.target.value.slice(0, 16) })} /></label>}
          <label htmlFor="maintenance-length">Lasts
            <select id="maintenance-length" value={draft.length === "duration" ? String(draft.durationMinutes) : draft.length} onChange={(event) => {
              const value = event.target.value;
              if (value === "until") update({ length: "until", endsAt: draft.endsAt || localInput((draft.later ? Date.parse(draft.startsAt) || now : now) + 2 * 60 * 60_000) });
              else if (value === "open") update({ length: "open" });
              else update({ length: "duration", durationMinutes: Number(value) });
            }}>
              {DURATION_CHOICES.map(({ minutes, label }) => <option key={minutes} value={minutes}>{label}</option>)}
              <option value="until">Until a time</option>
              <option value="open">Until someone ends it</option>
            </select>
          </label>
          {draft.length === "until" && <label htmlFor="maintenance-ends-at">Ends at<input id="maintenance-ends-at" type="datetime-local" required value={draft.endsAt} onChange={(event) => update({ endsAt: event.target.value.slice(0, 16) })} /></label>}
          <fieldset className="choice-list">
            <legend>Scheduled automation</legend>
            {AUTOMATIONS.map((automation) => (
              <label key={automation}>
                <input type="radio" name="maintenance-automation" checked={draft.automation === automation} onChange={() => update({ automation })} />
                <span><strong>{AUTOMATION_CHOICES[automation].label}</strong>{AUTOMATION_CHOICES[automation].help}</span>
              </label>
            ))}
          </fieldset>
          <small className="field-help">A policy&apos;s run that falls in the window is skipped, not saved for later, and its job shows the device as skipped for maintenance. Run now and jobs people start always run.</small>
        </fieldset>
        {(error || (problem && draft.reason.trim())) && <p role="alert" className="installer-error">{error ?? problem}</p>}
        <div className="connection-reason-actions">
          <button type="button" className="secondary-button" onClick={onClose}>Cancel</button>
          <button className="primary-button" disabled={busy || problem !== null}>{busy && <LoaderCircle size={16} className="spin" />} {draft.later ? "Schedule maintenance" : "Start maintenance"}</button>
        </div>
      </form>
    </ModalDialog>
  );
}
