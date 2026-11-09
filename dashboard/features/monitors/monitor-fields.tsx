import { useCallback } from "react";
import { useResource } from "../../lib/use-resource";
import { fetchToolbox } from "../toolbox/toolbox-api";
import type { ToolboxScript } from "../toolbox/model";
import { useWorkspace } from "../workspace/workspace-context";
import {
  EVENT_LEVELS,
  EVENT_LEVEL_LABELS,
  type EventLevel,
  INTERVAL_CHOICES,
  MAX_CERTIFICATE_DAYS,
  MAX_FAILURES,
  type MonitorCondition,
  type MonitorForm,
  PROCESS_ALERT_LABELS,
  type ProcessAlert,
  RUN_AS_LABELS,
  type RunAs,
  formatSeconds,
} from "./model";

// The shared toolbox scripts, which are the ones rules may run.
export function useSharedScripts(enabled: boolean) {
  const { authorizedFetch } = useWorkspace();
  const load = useCallback(
    async () => (enabled ? (await fetchToolbox(authorizedFetch)).scripts.filter((script) => script.shared) : []),
    [authorizedFetch, enabled],
  );
  const { data, error } = useResource(load, "The toolbox’s scripts could not be loaded.");
  return { scripts: data, error };
}

export function ScriptChoice({ id, label, value, scripts, error, onChange }: {
  id: string;
  label: string;
  value: string;
  scripts: ToolboxScript[] | null;
  error: string | null;
  onChange: (scriptId: string) => void;
}) {
  const known = scripts?.some((script) => script.id === value);
  return (
    <label htmlFor={id}>{label}
      <select id={id} value={value} required onChange={(event) => onChange(event.target.value)}>
        <option value="">{scripts ? (scripts.length ? "Choose a script" : "No shared scripts yet") : error ? "Couldn’t load the scripts" : "Loading…"}</option>
        {value && scripts && !known && <option value={value}>A script you can’t see</option>}
        {scripts?.map((script) => <option key={script.id} value={script.id}>{script.folder ? `${script.folder}/${script.name}` : script.name}</option>)}
      </select>
    </label>
  );
}

export function RunAsChoice({ id, value, onChange }: { id: string; value: RunAs; onChange: (runAs: RunAs) => void }) {
  return (
    <label htmlFor={id}>Run as
      <select id={id} value={value} onChange={(event) => onChange(event.target.value as RunAs)}>
        {(Object.keys(RUN_AS_LABELS) as RunAs[]).map((runAs) => <option key={runAs} value={runAs}>{RUN_AS_LABELS[runAs]}</option>)}
      </select>
    </label>
  );
}

const SERVICE_PLACEHOLDER = "Spooler, nginx or com.example.backup";

// What a monitor rule’s Agent checks, how often, and how many failures in
// a row raise its alert.
export function MonitorFields({ condition, form, scripts, scriptsError, onChange }: {
  condition: MonitorCondition;
  form: MonitorForm;
  scripts: ToolboxScript[] | null;
  scriptsError: string | null;
  onChange: (change: Partial<MonitorForm>) => void;
}) {
  const intervals = INTERVAL_CHOICES.includes(form.interval) ? INTERVAL_CHOICES : [...INTERVAL_CHOICES, form.interval].sort((left, right) => left - right);
  const load = form.alertWhen === "cpu_above" || form.alertWhen === "memory_above";
  const toggleLevel = (level: EventLevel, checked: boolean) =>
    onChange({ levels: checked ? EVENT_LEVELS.filter((candidate) => candidate === level || form.levels.includes(candidate)) : form.levels.filter((candidate) => candidate !== level) });

  return (
    <>
      {condition === "service" && (
        <>
          <label htmlFor="monitor-name">Service<input id="monitor-name" required maxLength={256} value={form.name} onChange={(event) => onChange({ name: event.target.value })} placeholder={SERVICE_PLACEHOLDER} /></label>
          <p className="field-help">A Windows service’s name or display name, a systemd unit, or a launchd job’s label in the system domain.</p>
          <label className="checkbox-row">
            <input type="checkbox" checked={form.automatic} onChange={(event) => onChange({ automatic: event.target.checked })} />
            <span><strong>Also alert if it isn’t set to start automatically</strong>On Windows the startup type is Automatic; on Linux the unit is enabled; on a Mac the job isn’t disabled.</span>
          </label>
        </>
      )}
      {condition === "process" && (
        <>
          <div className="script-editor-row">
            <label htmlFor="monitor-name">Process<input id="monitor-name" required maxLength={256} value={form.name} onChange={(event) => onChange({ name: event.target.value })} placeholder="sqlservr.exe or postgres" /></label>
            <label htmlFor="monitor-alert-when">Alert when
              <select id="monitor-alert-when" value={form.alertWhen} onChange={(event) => onChange({ alertWhen: event.target.value as ProcessAlert })}>
                {(Object.keys(PROCESS_ALERT_LABELS) as ProcessAlert[]).map((choice) => <option key={choice} value={choice}>{PROCESS_ALERT_LABELS[choice]}</option>)}
              </select>
            </label>
          </div>
          {load && (
            <div className="script-editor-row">
              {form.alertWhen === "cpu_above" ? (
                <label htmlFor="monitor-percent">Above
                  <span className="input-with-unit"><input id="monitor-percent" type="number" inputMode="numeric" min={1} max={99} required value={form.percent} onChange={(event) => onChange({ percent: event.target.value })} /><span aria-hidden="true">% CPU</span></span>
                </label>
              ) : (
                <label htmlFor="monitor-megabytes">Above
                  <span className="input-with-unit"><input id="monitor-megabytes" type="number" inputMode="numeric" min={1} required value={form.megabytes} onChange={(event) => onChange({ megabytes: event.target.value })} /><span aria-hidden="true">MB</span></span>
                </label>
              )}
              <label htmlFor="monitor-minutes">For at least
                <span className="input-with-unit"><input id="monitor-minutes" type="number" inputMode="numeric" min={1} required value={form.minutes} onChange={(event) => onChange({ minutes: event.target.value })} /><span aria-hidden="true">minutes</span></span>
              </label>
            </div>
          )}
          <p className="field-help">The program’s file name, with or without .exe on Windows. Processes of the same name count together; CPU is a share of all the device’s cores.</p>
        </>
      )}
      {condition === "event_log" && (
        <>
          <div className="script-editor-row">
            <label htmlFor="monitor-log">Windows log<input id="monitor-log" maxLength={256} value={form.log} onChange={(event) => onChange({ log: event.target.value })} placeholder="System" /></label>
            <label htmlFor="monitor-source">Source<input id="monitor-source" maxLength={256} value={form.source} onChange={(event) => onChange({ source: event.target.value })} placeholder="Any source" /></label>
          </div>
          <p className="field-help">Windows reads the named log, and matches the source against the event’s provider. Linux reads the journal and matches the source against the unit, syslog identifier or command; a Mac reads the unified log and matches its subsystem or process. Leave the log empty to skip Windows devices.</p>
          <div className="script-editor-row">
            <label htmlFor="monitor-event-ids">Event IDs<input id="monitor-event-ids" value={form.eventIds} onChange={(event) => onChange({ eventIds: event.target.value })} placeholder="Any, or 7, 11, 51" /></label>
            <label htmlFor="monitor-message">Message contains<input id="monitor-message" maxLength={256} value={form.message} onChange={(event) => onChange({ message: event.target.value })} placeholder="Any message" /></label>
          </div>
          <fieldset className="choice-list">
            <legend>Levels</legend>
            {EVENT_LEVELS.map((level) => (
              <label key={level}><input type="checkbox" checked={form.levels.includes(level)} onChange={(event) => toggleLevel(level, event.target.checked)} /><strong>{EVENT_LEVEL_LABELS[level]}</strong></label>
            ))}
          </fieldset>
          <p className="field-help">Event IDs are Windows only. Any matching entry since the last check fails it; with no level chosen, every level but debug counts.</p>
        </>
      )}
      {(condition === "ping" || condition === "tcp_port" || condition === "certificate") && (
        <div className="script-editor-row">
          <label htmlFor="monitor-host">Host<input id="monitor-host" required maxLength={253} value={form.host} onChange={(event) => onChange({ host: event.target.value })} placeholder="192.168.1.1 or mail.example.com" /></label>
          {condition !== "ping" && <label htmlFor="monitor-port">Port<input id="monitor-port" type="number" inputMode="numeric" min={1} max={65535} required value={form.port} onChange={(event) => onChange({ port: event.target.value })} /></label>}
        </div>
      )}
      {condition === "certificate" && (
        <label htmlFor="monitor-days">Alert when it expires within
          <span className="input-with-unit"><input id="monitor-days" type="number" inputMode="numeric" min={1} max={MAX_CERTIFICATE_DAYS} required value={form.days} onChange={(event) => onChange({ days: event.target.value })} /><span aria-hidden="true">days</span></span>
        </label>
      )}
      {condition === "http" && (
        <>
          <label htmlFor="monitor-url">Address<input id="monitor-url" type="url" required maxLength={2048} value={form.url} onChange={(event) => onChange({ url: event.target.value })} placeholder="https://intranet.example.com/health" /></label>
          <div className="script-editor-row">
            <label htmlFor="monitor-status">Expected status<input id="monitor-status" type="number" inputMode="numeric" min={100} max={599} required value={form.status} onChange={(event) => onChange({ status: event.target.value })} /></label>
            <label htmlFor="monitor-keyword">Page contains<input id="monitor-keyword" maxLength={256} value={form.keyword} onChange={(event) => onChange({ keyword: event.target.value })} placeholder="Optional, matching case" /></label>
          </div>
          <label className="checkbox-row">
            <input type="checkbox" checked={form.verifyTls} onChange={(event) => onChange({ verifyTls: event.target.checked })} />
            <span><strong>Check the certificate</strong>Fail if the device doesn’t trust the server’s certificate. Redirects aren’t followed.</span>
          </label>
        </>
      )}
      {condition === "script_check" && (
        <div className="script-editor-row">
          <ScriptChoice id="monitor-script" label="Script" value={form.scriptId} scripts={scripts} error={scriptsError} onChange={(scriptId) => onChange({ scriptId })} />
          <RunAsChoice id="monitor-run-as" value={form.runAs} onChange={(runAs) => onChange({ runAs })} />
        </div>
      )}
      <div className="script-editor-row">
        <label htmlFor="monitor-interval">Check every
          <select id="monitor-interval" value={form.interval} onChange={(event) => onChange({ interval: Number(event.target.value) })}>
            {intervals.map((seconds) => <option key={seconds} value={seconds}>{formatSeconds(seconds)}</option>)}
          </select>
        </label>
        <label htmlFor="monitor-timeout">Time limit
          <span className="input-with-unit"><input id="monitor-timeout" type="number" inputMode="numeric" min={1} required value={form.timeout} onChange={(event) => onChange({ timeout: event.target.value })} /><span aria-hidden="true">seconds</span></span>
        </label>
        <label htmlFor="monitor-failures">Alert after
          <span className="input-with-unit"><input id="monitor-failures" type="number" inputMode="numeric" min={1} max={MAX_FAILURES} required value={form.failures} onChange={(event) => onChange({ failures: event.target.value })} /><span aria-hidden="true">failures in a row</span></span>
        </label>
      </div>
    </>
  );
}
