import type { ToolboxScript } from "../toolbox/model";
import { RunAsChoice, ScriptChoice } from "./monitor-fields";
import { MAX_ATTEMPTS, MAX_RETRY_MINUTES, REMEDIATION_LABELS, type RemediationForm, type RemediationKind } from "./model";

// What a rule does about its alerts until their condition passes, and when
// its channels hear of them.
export function RemediationFields({ form, choices, scripts, scriptsError, canRunScripts, hasChannels, onChange }: {
  form: RemediationForm;
  choices: RemediationKind[];
  scripts: ToolboxScript[] | null;
  scriptsError: string | null;
  // Self-healing runs with the saver’s access, so it needs scripts.run.
  canRunScripts: boolean;
  hasChannels: boolean;
  onChange: (change: Partial<RemediationForm>) => void;
}) {
  if (!choices.length) return null;
  const action = choices.includes(form.action) ? form.action : choices[0];
  return (
    <fieldset className="remediation-fields">
      <legend>Self-healing</legend>
      <label className="checkbox-row">
        <input type="checkbox" checked={form.enabled} disabled={!canRunScripts && !form.enabled} onChange={(event) => onChange({ enabled: event.target.checked, action })} />
        <span>
          <strong>Try to fix it automatically</strong>
          {canRunScripts
            ? "Each attempt runs with your access, and is recorded on the alert and in the audit log. If the problem clears, the alert resolves as fixed by self-healing."
            : "Needs the scripts.run permission: it runs with the access of whoever saves the rule."}
        </span>
      </label>
      {form.enabled && (
        <>
          <div className="script-editor-row">
            <label htmlFor="remediation-action">Action
              <select id="remediation-action" value={action} onChange={(event) => onChange({ action: event.target.value as RemediationKind })}>
                {choices.map((choice) => <option key={choice} value={choice}>{REMEDIATION_LABELS[choice]}</option>)}
              </select>
            </label>
            <label htmlFor="remediation-attempts">Attempts
              <span className="input-with-unit"><input id="remediation-attempts" type="number" inputMode="numeric" min={1} max={MAX_ATTEMPTS} required value={form.attempts} onChange={(event) => onChange({ attempts: event.target.value })} /><span aria-hidden="true">at most</span></span>
            </label>
            <label htmlFor="remediation-retry">Apart
              <span className="input-with-unit"><input id="remediation-retry" type="number" inputMode="numeric" min={1} max={MAX_RETRY_MINUTES} required value={form.retryMinutes} onChange={(event) => onChange({ retryMinutes: event.target.value })} /><span aria-hidden="true">minutes</span></span>
            </label>
          </div>
          {action === "script" && (
            <div className="script-editor-row">
              <ScriptChoice id="remediation-script" label="Script" value={form.scriptId} scripts={scripts} error={scriptsError} onChange={(scriptId) => onChange({ scriptId })} />
              <RunAsChoice id="remediation-run-as" value={form.runAs} onChange={(runAs) => onChange({ runAs })} />
            </div>
          )}
          {hasChannels && (
            <label className="checkbox-row">
              <input type="checkbox" checked={form.notifyWhen === "remediation_failed"} onChange={(event) => onChange({ notifyWhen: event.target.checked ? "remediation_failed" : "raised" })} />
              <span><strong>Notify only if self-healing fails</strong>Problems it fixes stay quiet; the channels hear once the last attempt hasn’t helped.</span>
            </label>
          )}
        </>
      )}
    </fieldset>
  );
}
