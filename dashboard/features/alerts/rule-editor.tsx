import { LoaderCircle, X } from "lucide-react";
import { type FormEvent, type RefObject, useId, useState } from "react";
import { AuthenticationRequired, errorText } from "../../lib/http";
import { ModalDialog } from "../../lib/modal-dialog";
import { useWorkspace } from "../workspace/workspace-context";
import { saveRule } from "./alerts-api";
import {
  type AlertRule,
  CONDITIONS,
  CONDITION_LABELS,
  type Channel,
  type Condition,
  MAX_DURATION,
  type RuleForm,
  SEVERITIES,
  SEVERITY_LABELS,
  MAX_DAYS,
  type Severity,
  type ThresholdUnit,
  UPDATE_CATEGORY_CHOICES,
  type UpdateCategoryChoice,
  ruleForm,
  ruleFormProblem,
  ruleInput,
  updateCategoryChoiceLabel,
  usesDays,
  usesDuration,
  usesThreshold,
  withCondition,
} from "./model";

const CONDITION_HELP: Record<Condition, string> = {
  device_offline: "The device's Agent has been disconnected this long. After the server restarts, devices count as offline from the restart.",
  disk_space: "A volume's free space is below the threshold. Each volume gets its own alert.",
  cpu: "The device's CPU load, averaged over the duration, is above the threshold.",
  memory: "The device's memory use, averaged over the duration, is above the threshold.",
  script_failed: "A toolbox script run on the device couldn't start, timed out, or never reported back. Its alert stays until someone resolves it.",
  updates_pending: "The device's last scan lists updates of the chosen kind that have been pending longer than this. It resolves itself once they're installed.",
  update_install_failed: "An install of operating system updates on the device failed, partly failed, or never reported back. Its alert stays until someone resolves it.",
  reboot_pending: "The device has needed a restart to finish installing updates for longer than this. It resolves itself after the restart.",
};

// Creates a rule, or edits one. A rule's condition can't change once saved.
export function RuleEditor({ rule, channels, returnFocus, onClose, onSaved }: {
  rule: AlertRule | null;
  channels: Channel[];
  returnFocus: RefObject<HTMLElement | null>;
  onClose: () => void;
  onSaved: (rule: AlertRule) => void;
}) {
  const { authorizedFetch, inventory } = useWorkspace();
  const titleId = useId();
  const [form, setForm] = useState<RuleForm>(() => ruleForm(rule));
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const problem = ruleFormProblem(form);
  const update = (change: Partial<RuleForm>) => setForm((current) => ({ ...current, ...change }));
  const toggle = (list: "companyIds" | "channelIds", id: string, checked: boolean) => {
    setForm((current) => ({
      ...current,
      [list]: checked ? [...current[list], id] : current[list].filter((candidate) => candidate !== id),
    }));
  };

  const submit = async (event: FormEvent) => {
    event.preventDefault();
    if (problem) return;
    setBusy(true);
    setError(null);
    try {
      onSaved(await saveRule(authorizedFetch, rule?.id ?? null, ruleInput(form)));
    } catch (failure) {
      if (!(failure instanceof AuthenticationRequired)) setError(errorText(failure, "The rule could not be saved."));
      setBusy(false);
    }
  };

  const gigabytes = form.condition === "disk_space" && form.thresholdUnit === "gb";
  const thresholdLabel = form.condition === "disk_space" ? "Alert when free space is below" : "Alert when the average is above";
  const { companies } = inventory;

  return (
    <ModalDialog className="settings-modal role-modal" labelledBy={titleId} onClose={onClose} returnFocus={returnFocus}>
      <button type="button" className="modal-close" onClick={onClose} aria-label="Close"><X size={19} /></button>
      <h2 id={titleId}>{rule ? `Edit ${rule.name}` : "New alert rule"}</h2>
      <form onSubmit={(event) => void submit(event)}>
        <fieldset className="script-editor-fields" disabled={busy}>
          <label htmlFor="rule-name">Name<input id="rule-name" required maxLength={120} value={form.name} onChange={(event) => update({ name: event.target.value })} placeholder="Servers offline" /></label>
          <div className="script-editor-row">
            <label htmlFor="rule-condition">Condition
              <select id="rule-condition" value={form.condition} disabled={rule !== null} onChange={(event) => setForm((current) => withCondition(current, event.target.value as Condition))}>
                {CONDITIONS.map((condition) => <option key={condition} value={condition}>{CONDITION_LABELS[condition]}</option>)}
              </select>
            </label>
            <label htmlFor="rule-severity">Severity
              <select id="rule-severity" value={form.severity} onChange={(event) => update({ severity: event.target.value as Severity })}>
                {SEVERITIES.map((severity) => <option key={severity} value={severity}>{SEVERITY_LABELS[severity]}</option>)}
              </select>
            </label>
          </div>
          <p className="field-help">{CONDITION_HELP[form.condition]}</p>
          {(usesThreshold(form.condition) || usesDuration(form.condition)) && (
            <div className="script-editor-row">
              {usesThreshold(form.condition) && (
                <label htmlFor="rule-threshold">{thresholdLabel}
                  <span className="input-with-unit">
                    <input id="rule-threshold" type="number" inputMode="decimal" min={0} max={gigabytes ? undefined : 100} step="any" required value={form.threshold} onChange={(event) => update({ threshold: event.target.value })} />
                    {form.condition === "disk_space" ? (
                      <select aria-label="Unit" value={form.thresholdUnit} onChange={(event) => update({ thresholdUnit: event.target.value as ThresholdUnit })}>
                        <option value="percent">%</option>
                        <option value="gb">GB</option>
                      </select>
                    ) : <span aria-hidden="true">%</span>}
                  </span>
                </label>
              )}
              {usesDuration(form.condition) && (
                <label htmlFor="rule-duration">{form.condition === "device_offline" ? "Offline for at least" : "Sustained for"}
                  <span className="input-with-unit">
                    <input id="rule-duration" type="number" inputMode="numeric" min={1} max={MAX_DURATION[form.condition]} step={1} required value={form.duration} onChange={(event) => update({ duration: event.target.value })} />
                    <span aria-hidden="true">minutes</span>
                  </span>
                </label>
              )}
            </div>
          )}
          {usesDays(form.condition) && (
            <div className="script-editor-row">
              {form.condition === "updates_pending" && (
                <label htmlFor="rule-update-category">Updates
                  <select id="rule-update-category" value={form.updateCategory} onChange={(event) => update({ updateCategory: event.target.value as UpdateCategoryChoice })}>
                    {UPDATE_CATEGORY_CHOICES.map((choice) => <option key={choice} value={choice}>{updateCategoryChoiceLabel(choice)}</option>)}
                  </select>
                </label>
              )}
              <label htmlFor="rule-days">Pending for more than
                <span className="input-with-unit">
                  <input id="rule-days" type="number" inputMode="numeric" min={1} max={MAX_DAYS} step={1} required value={form.days} onChange={(event) => update({ days: event.target.value })} />
                  <span aria-hidden="true">days</span>
                </span>
              </label>
            </div>
          )}
          {form.condition === "script_failed" && (
            <label className="checkbox-row">
              <input type="checkbox" checked={form.nonzeroExitFails} onChange={(event) => update({ nonzeroExitFails: event.target.checked })} />
              <span><strong>Count exit codes other than 0 as failures</strong>A script that runs to the end but exits with an error code raises the alert too.</span>
            </label>
          )}

          <fieldset className="choice-list">
            <legend>Applies to</legend>
            <label><input type="radio" name="rule-scope" checked={form.allCompanies} onChange={() => update({ allCompanies: true })} /><span><strong>Every device</strong>Including devices in no company.</span></label>
            <label><input type="radio" name="rule-scope" checked={!form.allCompanies} onChange={() => update({ allCompanies: false })} disabled={!companies.length && form.allCompanies} /><span><strong>Devices of chosen companies</strong>{companies.length ? "Choose them below." : "There are no companies yet."}</span></label>
          </fieldset>
          {!form.allCompanies && (
            <fieldset className="choice-list">
              <legend>Companies</legend>
              {companies.map((company) => (
                <label key={company.id}>
                  <input type="checkbox" checked={form.companyIds.includes(company.id)} onChange={(event) => toggle("companyIds", company.id, event.target.checked)} />
                  <strong>{company.name}</strong>
                </label>
              ))}
            </fieldset>
          )}

          <fieldset className="choice-list">
            <legend>Notify</legend>
            {channels.length === 0 && <p className="field-help">No notification channels yet. Add one on the Notification channels tab; until then, alerts only show here.</p>}
            {channels.map((channel) => (
              <label key={channel.id}>
                <input type="checkbox" checked={form.channelIds.includes(channel.id)} onChange={(event) => toggle("channelIds", channel.id, event.target.checked)} />
                <span><strong>{channel.name}{!channel.enabled && <span className="badge badge-muted">Off</span>}</strong>{channel.kind === "email" ? "Email" : "Webhook"}</span>
              </label>
            ))}
          </fieldset>
          {form.channelIds.length > 0 && (
            <label className="checkbox-row">
              <input type="checkbox" checked={form.notifyOnResolve} onChange={(event) => update({ notifyOnResolve: event.target.checked })} />
              <span><strong>Notify when alerts resolve too</strong>Otherwise only raised alerts notify.</span>
            </label>
          )}
          <label className="checkbox-row">
            <input type="checkbox" checked={form.enabled} onChange={(event) => update({ enabled: event.target.checked })} />
            <span><strong>Turned on</strong>A rule that&apos;s off raises nothing, and its unresolved alerts resolve.</span>
          </label>
        </fieldset>
        {(error || (problem && form.name.trim())) && <p role="alert" className="installer-error">{error ?? problem}</p>}
        <div className="connection-reason-actions">
          <button type="button" className="secondary-button" onClick={onClose}>Cancel</button>
          <button className="primary-button" disabled={busy || problem !== null}>{busy && <LoaderCircle size={16} className="spin" />} {rule ? "Save rule" : "Create rule"}</button>
        </div>
      </form>
    </ModalDialog>
  );
}
