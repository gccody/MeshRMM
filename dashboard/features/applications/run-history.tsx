import { ChevronDown, ChevronRight, CircleAlert, LoaderCircle, RotateCcw } from "lucide-react";
import { Fragment, useCallback, useState } from "react";
import { Link } from "react-router";
import { formatDateTime, formatRelative } from "../../lib/format";
import { useResource } from "../../lib/use-resource";
import { useWorkspace } from "../workspace/workspace-context";
import { fetchApplicationRun, fetchApplicationRuns } from "./applications-api";
import { type ApplicationRun, OUTCOME_LABELS, RUN_STATUS_LABELS, SOURCE_LABELS, itemTone, runCounts, runTone } from "./model";

const HISTORY_LIMIT = 25;

// The device's installs, updates, uninstalls and scans, newest first; each
// opens to show how every application went and what its installer printed.
export function RunHistory({ deviceId, reloadKey }: { deviceId: string; reloadKey: number }) {
  const { authorizedFetch } = useWorkspace();
  // `reloadKey` is in the dependencies so a finished run reads the list again.
  // eslint-disable-next-line react-hooks/exhaustive-deps
  const load = useCallback(() => fetchApplicationRuns(authorizedFetch, { deviceId, limit: HISTORY_LIMIT }), [authorizedFetch, deviceId, reloadKey]);
  const { data, error } = useResource(load, "The application history could not be loaded.");
  const [open, setOpen] = useState<string | null>(null);
  const runs = data?.filter((run) => run.action !== "scan" || run.status !== "succeeded") ?? null;

  return (
    <section className="agent-panel">
      <div className="panel-heading"><h2>History</h2></div>
      {!runs ? (
        error ? <p className="table-message form-error" role="alert"><CircleAlert size={16} aria-hidden="true" />{error}</p>
          : <p className="table-message"><LoaderCircle size={16} className="spin" aria-hidden="true" /> Loading the history…</p>
      ) : runs.length === 0 ? (
        <p className="table-message field-help">Nothing has been installed, updated or uninstalled on this device yet.</p>
      ) : (
        <div className="table-scroll">
          <table className="data-table" aria-label="Application history">
            <thead>
              <tr>
                <th scope="col">What</th>
                <th scope="col">Result</th>
                <th scope="col">Started</th>
                <th scope="col">By</th>
              </tr>
            </thead>
            <tbody>
              {runs.map((run) => (
                <Fragment key={run.id}>
                  <tr>
                    <td>
                      <button type="button" className="link-button" aria-expanded={open === run.id} onClick={() => setOpen(open === run.id ? null : run.id)}>
                        {open === run.id ? <ChevronDown size={14} aria-hidden="true" /> : <ChevronRight size={14} aria-hidden="true" />}
                        {run.summary}
                      </button>
                    </td>
                    <td>
                      <span className={`run-pill run-pill-${runTone(run.status)}`}>{RUN_STATUS_LABELS[run.status]}</span>
                      {run.status !== "pending" && run.action !== "scan" && runCounts(run) && <span>{runCounts(run)}</span>}
                      {run.reboot_required && <span className="badge badge-warn"><RotateCcw size={11} aria-hidden="true" />Restart needed</span>}
                    </td>
                    <td title={formatDateTime(run.created_at)}>{formatRelative(run.created_at)}</td>
                    <td>{run.policy_name ? `Policy “${run.policy_name}”` : run.requested_by}{run.job_id && <> · <Link to={`/job?id=${encodeURIComponent(run.job_id)}`}>Job</Link></>}</td>
                  </tr>
                  {open === run.id && <tr><td colSpan={4}><RunDetail run={run} /></td></tr>}
                </Fragment>
              ))}
            </tbody>
          </table>
        </div>
      )}
    </section>
  );
}

// One run's applications, with their output, read when opened.
export function RunDetail({ run }: { run: ApplicationRun }) {
  const { authorizedFetch } = useWorkspace();
  const load = useCallback(() => fetchApplicationRun(authorizedFetch, run.id), [authorizedFetch, run.id]);
  const { data: detail, error } = useResource(load, "The run could not be loaded.");
  const items = detail?.items ?? run.items ?? [];
  return (
    <div className="run-detail">
      {run.error && <p className="error-text">{run.error}</p>}
      {run.progress && run.status === "pending" && <p className="field-help">{run.progress}</p>}
      {error && <p className="form-error" role="alert">{error}</p>}
      {!detail && !error && <p className="field-help"><LoaderCircle size={14} className="spin" aria-hidden="true" /> Loading…</p>}
      {items.length > 0 && (
        <ul className="run-items">
          {items.map((item) => (
            <li key={item.position}>
              <strong>{item.name}</strong>
              <span className={`run-pill run-pill-${itemTone(item.outcome)}`}>{OUTCOME_LABELS[item.outcome]}</span>
              {item.source && <span className="mono-detail">{SOURCE_LABELS[item.source]}{item.package_id ? ` ${item.package_id}` : ""}</span>}
              {item.version && <span>Version {item.version}</span>}
              {item.exit_code !== null && <span>Exit code {item.exit_code}</span>}
              {item.reboot_required && <span className="badge badge-warn">Restart needed</span>}
              {item.error && <span className="error-text">{item.error}</span>}
              {item.output && <details><summary>Output</summary><pre className="application-output">{item.output}</pre></details>}
            </li>
          ))}
        </ul>
      )}
    </div>
  );
}
