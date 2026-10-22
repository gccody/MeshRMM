import { LoaderCircle, X } from "lucide-react";
import { type FormEvent, type RefObject, useId, useMemo, useState } from "react";
import { AuthenticationRequired, errorText } from "../../lib/http";
import { ModalDialog } from "../../lib/modal-dialog";
import type { Channel } from "../alerts/model";
import { LANGUAGE_LABELS, RUN_AS_LABELS, type ToolboxScript, groupByFolder } from "../toolbox/model";
import { CategoryChoice } from "../updates/bulk-install-dialog";
import { RebootFields } from "../updates/reboot-fields";
import { useWorkspace } from "../workspace/workspace-context";
import { savePolicy } from "./automation-api";
import { DevicePicker } from "./device-picker";
import {
  ACTION_LABELS,
  MAX_WAIT_HOURS,
  type Policy,
  type PolicyAction,
  SAVE_PERMISSION,
  type PolicyDraft,
  TRIGGER_LABELS,
  type TriggerKind,
  WEEKDAY_LABELS,
  currentZoneName,
  policyDraft,
  policyDraftProblem,
  policyInput,
  timeZones,
} from "./model";

const TRIGGERS = Object.keys(TRIGGER_LABELS) as TriggerKind[];
const ACTIONS = Object.keys(ACTION_LABELS) as PolicyAction[];

const ACTION_INTROS: Record<PolicyAction, string> = {
  script: "The server runs the policy's script by itself, as a job.",
  update_scan: "The server asks the devices to look for operating system updates, as a job, so their lists stay current between the server's own scans.",
  update_install: "The server has the devices install pending operating system updates of the chosen kinds, as a job, and restart as you choose.",
  wake: "The server wakes the devices that are offline with Wake-on-LAN, as a job, through online devices on their network: before a patch window, for example.",
};

const browserZone = () => currentZoneName(Intl.DateTimeFormat().resolvedOptions().timeZone || "UTC");

// Creates a policy, or edits one. Saving makes the user its owner: it runs
// with their access from then on.
export function PolicyEditor({ policy, scripts, channels, returnFocus, onClose, onSaved }: {
  policy: Policy | null;
  // Shared scripts only; policies run nothing else.
  scripts: ToolboxScript[];
  // Null when the user may not see notification channels.
  channels: Channel[] | null;
  returnFocus: RefObject<HTMLElement | null>;
  onClose: () => void;
  onSaved: (policy: Policy) => void;
}) {
  const { authorizedFetch, inventory, can } = useWorkspace();
  const titleId = useId();
  const zones = useMemo(() => timeZones(Intl.supportedValuesOf("timeZone")), []);
  // The actions the user may save: a policy runs with its saver's access.
  const actions = ACTIONS.filter((action) => can(SAVE_PERMISSION[action]) || policy?.action === action);
  const [draft, setDraft] = useState<PolicyDraft>(() => policyDraft(policy, browserZone(), actions[0] ?? "script"));
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const problem = policyDraftProblem(draft, zones);
  const update = (change: Partial<PolicyDraft>) => setDraft((current) => ({ ...current, ...change }));
  const toggle = (list: "companyIds" | "channelIds", id: string, checked: boolean) => {
    setDraft((current) => ({ ...current, [list]: checked ? [...current[list], id] : current[list].filter((candidate) => candidate !== id) }));
  };
  const script = scripts.find((candidate) => candidate.id === draft.scriptId);
  // A script no longer shared stays shown, so the user sees why it can't save.
  const missingScript = policy && draft.scriptId === policy.script_id && !script;
  const folders = groupByFolder(scripts);
  const usesZone = draft.trigger === "daily" || draft.trigger === "weekly" || draft.trigger === "once";
  const enrolling = draft.trigger === "enroll";
  const isScript = draft.action === "script";
  const isWake = draft.action === "wake";
  const triggers = isWake ? TRIGGERS.filter((trigger) => trigger !== "enroll") : TRIGGERS;
  const missingPermission = can(SAVE_PERMISSION[draft.action]) ? null : `Saving a policy that does this needs the ${SAVE_PERMISSION[draft.action]} permission.`;

  const submit = async (event: FormEvent) => {
    event.preventDefault();
    if (problem || missingPermission) return;
    setBusy(true);
    setError(null);
    try {
      onSaved(await savePolicy(authorizedFetch, policy?.id ?? null, policyInput(draft)));
    } catch (failure) {
      if (!(failure instanceof AuthenticationRequired)) setError(errorText(failure, "The policy could not be saved."));
      setBusy(false);
    }
  };

  return (
    <ModalDialog className="settings-modal role-modal" labelledBy={titleId} onClose={onClose} returnFocus={returnFocus}>
      <button type="button" className="modal-close" onClick={onClose} aria-label="Close"><X size={19} /></button>
      <h2 id={titleId}>{policy ? `Edit ${policy.name}` : "New policy"}</h2>
      <p>{ACTION_INTROS[draft.action]} It runs with your access: saving makes you its owner{policy && !policy.owned_by_you ? ` in place of ${policy.owner}` : ""}.</p>
      <form onSubmit={(event) => void submit(event)}>
        <fieldset className="script-editor-fields" disabled={busy}>
          <label htmlFor="policy-name">Name<input id="policy-name" required maxLength={120} value={draft.name} onChange={(event) => update({ name: event.target.value })} placeholder="Nightly cleanup" /></label>
          {actions.length > 1 && (
            <div className="segmented action-choice" role="group" aria-label="What the policy does">
              {actions.map((action) => <button key={action} type="button" aria-pressed={draft.action === action} onClick={() => update(action === "wake" && draft.trigger === "enroll" ? { action, trigger: "daily" } : { action })}>{ACTION_LABELS[action]}</button>)}
            </div>
          )}
          {isScript && <>
          <label htmlFor="policy-script">Script
            <select id="policy-script" value={draft.scriptId} onChange={(event) => update({ scriptId: event.target.value })}>
              <option value="" disabled>{scripts.length ? "Choose a shared script" : "No shared scripts yet"}</option>
              {missingScript && <option value={draft.scriptId} disabled>{policy?.script_name ?? "Deleted script"} (no longer shared)</option>}
              {folders.map(({ folder, items }) => (
                <optgroup key={folder} label={folder || "Top level"}>
                  {items.map((item) => <option key={item.id} value={item.id}>{item.name} ({LANGUAGE_LABELS[item.language]})</option>)}
                </optgroup>
              ))}
            </select>
          </label>
          <small className="field-help">Policies run shared scripts only, so everyone who manages policies can see what each one runs. Devices without the script&apos;s interpreter are skipped.</small>
          <fieldset className="run-as-choice">
            <legend>Run as</legend>
            {(["system", "user"] as const).map((choice) => (
              <label key={choice}>
                <input type="radio" name="policy-run-as" value={choice} checked={draft.runAs === choice} onChange={() => update({ runAs: choice })} />
                <span><strong>{RUN_AS_LABELS[choice]}</strong>{choice === "user" ? "The person signed in to each device. Where nobody is, the system account." : "SYSTEM on Windows, root on macOS and Linux."}</span>
              </label>
            ))}
          </fieldset>
          </>}
          {draft.action === "update_install" && <>
            <CategoryChoice categories={draft.updateCategories} onToggle={(category, checked) => update({ updateCategories: checked ? [...draft.updateCategories, category] : draft.updateCategories.filter((candidate) => candidate !== category) })} />
            <RebootFields name="policy-reboot" reboot={draft.reboot} delay={draft.rebootDelayMinutes} onReboot={(reboot) => update({ reboot })} onDelay={(rebootDelayMinutes) => update({ rebootDelayMinutes })} />
          </>}
          {isWake && <p className="field-help">Online devices are skipped. A device that comes online within 10 minutes of its packets counts as woken; one that no online device shares a network with fails. Run it a few minutes before the work that needs the devices.</p>}
          {draft.action === "update_scan" && <p className="field-help">Devices also scan on the server&apos;s own schedule, set under Settings. A scan policy adds scans at the times you choose.</p>}

          <label htmlFor="policy-trigger">Runs
            <select id="policy-trigger" value={draft.trigger} onChange={(event) => {
              const trigger = event.target.value as TriggerKind;
              update(trigger === "enroll" && draft.target === "devices" ? { trigger, target: "all" } : { trigger });
            }}>
              {triggers.map((trigger) => <option key={trigger} value={trigger}>{TRIGGER_LABELS[trigger]}</option>)}
            </select>
          </label>
          {draft.trigger === "interval" && (
            <label htmlFor="policy-interval">Every
              <span className="input-with-unit">
                <input id="policy-interval" type="number" inputMode="numeric" min={1} step={1} required value={draft.intervalValue} onChange={(event) => update({ intervalValue: event.target.value })} />
                <select aria-label="Unit" value={draft.intervalUnit} onChange={(event) => update({ intervalUnit: event.target.value as PolicyDraft["intervalUnit"] })}>
                  <option value="minutes">minutes</option>
                  <option value="hours">hours</option>
                </select>
              </span>
            </label>
          )}
          {draft.trigger === "weekly" && (
            <fieldset className="weekday-choice">
              <legend>On</legend>
              {WEEKDAY_LABELS.map((label, index) => (
                <label key={label}>
                  <input type="checkbox" checked={draft.weekdays.includes(index + 1)} onChange={(event) => update({ weekdays: event.target.checked ? [...draft.weekdays, index + 1] : draft.weekdays.filter((day) => day !== index + 1) })} />
                  <span>{label}</span>
                </label>
              ))}
            </fieldset>
          )}
          {usesZone && (
            <div className="script-editor-row">
              {draft.trigger === "once"
                ? <label htmlFor="policy-run-at">On<input id="policy-run-at" type="datetime-local" required value={draft.runAt} onChange={(event) => update({ runAt: event.target.value.slice(0, 16) })} /></label>
                : <label htmlFor="policy-time">At<input id="policy-time" type="time" required value={draft.time} onChange={(event) => update({ time: event.target.value.slice(0, 5) })} /></label>}
              <label htmlFor="policy-zone">Time zone
                <select id="policy-zone" value={draft.timeZone} onChange={(event) => update({ timeZone: event.target.value })}>
                  {!zones.includes(draft.timeZone) && <option value={draft.timeZone}>{draft.timeZone}</option>}
                  {zones.map((zone) => <option key={zone} value={zone}>{zone}</option>)}
                </select>
              </label>
            </div>
          )}
          {draft.trigger === "interval" && <p className="field-help">Counts from when the policy is saved or turned on.</p>}
          {enrolling && <p className="field-help">Runs once on each device that enrolls from now on, when its Agent first connects.</p>}

          <fieldset className="choice-list">
            <legend>Devices</legend>
            <label><input type="radio" name="policy-target" checked={draft.target === "all"} onChange={() => update({ target: "all" })} /><span><strong>Every device</strong>As they are each time it runs.</span></label>
            <label><input type="radio" name="policy-target" checked={draft.target === "companies"} onChange={() => update({ target: "companies" })} disabled={!inventory.companies.length && draft.target !== "companies"} /><span><strong>Devices of chosen companies</strong>{inventory.companies.length ? "Choose them below." : "There are no companies yet."}</span></label>
            {!enrolling && <label><input type="radio" name="policy-target" checked={draft.target === "devices"} onChange={() => update({ target: "devices" })} /><span><strong>Chosen devices</strong>{draft.deviceIds.length ? `${draft.deviceIds.length} chosen` : "Choose them below."}</span></label>}
          </fieldset>
          {draft.target === "companies" && (
            <fieldset className="choice-list picker-list">
              <legend className="sr-only">Companies</legend>
              {inventory.companies.map((company) => (
                <label key={company.id}>
                  <input type="checkbox" checked={draft.companyIds.includes(company.id)} onChange={(event) => toggle("companyIds", company.id, event.target.checked)} />
                  <strong>{company.name}</strong>
                </label>
              ))}
            </fieldset>
          )}
          {draft.target === "devices" && <DevicePicker agents={inventory.agents} chosen={draft.deviceIds} onChange={(deviceIds) => update({ deviceIds: [...deviceIds] })} language={isScript ? script?.language ?? null : null} />}

          {!enrolling && !isWake && (
            <fieldset className="choice-list">
              <legend>Offline devices</legend>
              <label><input type="radio" name="policy-offline" checked={draft.offline === "wait"} onChange={() => update({ offline: "wait" })} /><span><strong>Catch them up</strong>Run when they connect within the time below, unless the policy runs again first.</span></label>
              <label><input type="radio" name="policy-offline" checked={draft.offline === "skip"} onChange={() => update({ offline: "skip" })} /><span><strong>Skip them</strong>They wait for the next run.</span></label>
            </fieldset>
          )}
          {!isWake && (enrolling || draft.offline === "wait") && (
            <label htmlFor="policy-catch-up">{enrolling ? "Wait for the new device up to" : "Catch up within"}
              <span className="input-with-unit">
                <input id="policy-catch-up" type="number" inputMode="numeric" min={1} max={MAX_WAIT_HOURS} step={1} required value={draft.catchUpHours} onChange={(event) => update({ catchUpHours: event.target.value })} />
                <span aria-hidden="true">hours</span>
              </span>
            </label>
          )}

          {channels === null ? (
            <p className="field-help">You can&apos;t see notification channels, so this policy can&apos;t notify anyone of failures. Someone with the alerts.manage permission can add them.</p>
          ) : (
            <fieldset className="choice-list">
              <legend>When a run finishes with failures, notify</legend>
              {channels.length === 0 && <p className="field-help">No notification channels yet. Add one under Alerts, on the Notification channels tab.</p>}
              {channels.map((channel) => (
                <label key={channel.id}>
                  <input type="checkbox" checked={draft.channelIds.includes(channel.id)} onChange={(event) => toggle("channelIds", channel.id, event.target.checked)} />
                  <span><strong>{channel.name}{!channel.enabled && <span className="badge badge-muted">Off</span>}</strong>{channel.kind === "email" ? "Email" : "Webhook"}</span>
                </label>
              ))}
            </fieldset>
          )}
          <label className="checkbox-row">
            <input type="checkbox" checked={draft.enabled} onChange={(event) => update({ enabled: event.target.checked })} />
            <span><strong>Turned on</strong>A policy that&apos;s off doesn&apos;t run; turning it on again starts its schedule over.</span>
          </label>
        </fieldset>
        {(error || (problem && draft.name.trim()) || missingPermission) && <p role="alert" className="installer-error">{error ?? missingPermission ?? problem}</p>}
        <div className="connection-reason-actions">
          <button type="button" className="secondary-button" onClick={onClose}>Cancel</button>
          <button className="primary-button" disabled={busy || problem !== null || missingPermission !== null}>{busy && <LoaderCircle size={16} className="spin" />} {policy ? "Save policy" : "Create policy"}</button>
        </div>
      </form>
    </ModalDialog>
  );
}
