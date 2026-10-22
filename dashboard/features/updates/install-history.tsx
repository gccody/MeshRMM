import { ChevronDown, ChevronRight, CircleAlert, LoaderCircle } from "lucide-react";
import { Fragment, useCallback, useEffect, useState } from "react";
import { Link } from "react-router";
import { formatDateTime } from "../../lib/format";
import { AuthenticationRequired, errorText } from "../../lib/http";
import { useWorkspace } from "../workspace/workspace-context";
import {
  CATEGORY_LABELS,
  RUN_STATUS_LABELS,
  type UpdateRun,
  applyRunUpdate,
  describeReboot,
  describeSelection,
  runRebootText,
  runStarter,
  runSummary,
  runTone,
} from "./model";
import { fetchRuns } from "./updates-api";
import { useUpdateEvents } from "./use-update-feed";

const HISTORY_LIMIT = 25;

// A device's installs, newest first, each with how every update went.
export function InstallHistory({ deviceId, reloadKey }: { deviceId: string; reloadKey: number }) {
  const { authorizedFetch } = useWorkspace();
  const [state, setState] = useState<{ runs: UpdateRun[] | null; error: string | null }>({ runs: null, error: null });
  const [open, setOpen] = useState<string | null>(null);

  const load = useCallback(() => fetchRuns(authorizedFetch, { deviceId, kind: "install", limit: HISTORY_LIMIT }), [authorizedFetch, deviceId]);
  useEffect(() => {
    let cancelled = false;
    load().then(
      (runs) => !cancelled && setState({ runs, error: null }),
      (error: unknown) => {
        if (!cancelled && !(error instanceof AuthenticationRequired)) setState((current) => ({ runs: current.runs, error: errorText(error, "The update history could not be loaded.") }));
      },
    );
    return () => {
      cancelled = true;
    };
  }, [load, reloadKey]);

  // Progress and results show at once; a finished install's updates come
  // with the next read, which `reloadKey` brings.
  useUpdateEvents((event) => {
    if (event.type !== "update_run" || event.run.device_id !== deviceId || event.run.kind !== "install") return;
    setState((current) => (current.runs ? { ...current, runs: applyRunUpdate(current.runs, event.run) } : current));
  });

  return (
    <section className="agent-panel">
      <div className="panel-header"><div><h2>Install history</h2></div></div>
      {state.error && <p className="table-message form-error" role="alert"><CircleAlert size={16} aria-hidden="true" />{state.error}</p>}
      {state.runs === null ? (
        !state.error && <p className="table-message page-status" role="status"><LoaderCircle size={16} className="spin" aria-hidden="true" /> Loading the history…</p>
      ) : state.runs.length === 0 ? (
        <p className="table-message field-help">No updates installed through MeshRMM yet. Installs are kept for a year.</p>
      ) : (
        <div className="table-scroll">
          <table className="data-table install-history" aria-label="Install history">
            <thead>
              <tr>
                <th scope="col">Started</th>
                <th scope="col">Result</th>
                <th scope="col">Updates</th>
                <th scope="col">Started by</th>
                <th scope="col">Restart</th>
              </tr>
            </thead>
            <tbody>
              {state.runs.map((run) => {
                const expanded = open === run.id;
                const items = run.items ?? [];
                const detailsId = `install-${run.id}`;
                const rebootText = runRebootText(run, formatDateTime);
                return (
                  <Fragment key={run.id}>
                    <tr>
                      <td>
                        {items.length > 0 ? (
                          <button type="button" className="disclosure-button" onClick={() => setOpen(expanded ? null : run.id)} aria-expanded={expanded} aria-controls={expanded ? detailsId : undefined}>
                            {expanded ? <ChevronDown size={15} aria-hidden="true" /> : <ChevronRight size={15} aria-hidden="true" />}
                            <strong>{formatDateTime(run.created_at)}</strong>
                          </button>
                        ) : <strong>{formatDateTime(run.created_at)}</strong>}
                        {run.completed_at && <span className="row-detail">Finished {formatDateTime(run.completed_at)}</span>}
                      </td>
                      <td>
                        <span className={`run-pill run-pill-${runTone(run.status)}`}>{RUN_STATUS_LABELS[run.status]}</span>
                        <span>{run.status === "pending" && run.progress ? run.progress : runSummary(run)}</span>
                        {run.error && run.status !== "pending" && (run.installed > 0 || run.failed > 0) && <span>{run.error}</span>}
                      </td>
                      <td>{describeSelection(run)}</td>
                      <td>
                        {run.job_id ? <Link to={`/job?id=${encodeURIComponent(run.job_id)}`}>{runStarter(run)}</Link> : runStarter(run)}
                      </td>
                      <td>
                        {describeReboot(run.reboot, run.reboot_delay_minutes)}
                        {rebootText && <span>{rebootText}</span>}
                      </td>
                    </tr>
                    {expanded && (
                      <tr className="detail-row" id={detailsId}>
                        <td colSpan={5}>
                          <table className="data-table nested-table">
                            <thead><tr><th scope="col">Update</th><th scope="col">Category</th><th scope="col">Result</th></tr></thead>
                            <tbody>
                              {items.map((item) => (
                                <tr key={item.update_id}>
                                  <td><strong>{item.title}</strong>{item.kb && <span>{item.kb}</span>}</td>
                                  <td>{CATEGORY_LABELS[item.category] ?? item.category}</td>
                                  <td>
                                    <span className={`run-pill run-pill-${item.outcome === "installed" ? "success" : "problem"}`}>{item.outcome === "installed" ? "Installed" : "Failed"}</span>
                                    {item.error && <span>{item.error}</span>}
                                  </td>
                                </tr>
                              ))}
                            </tbody>
                          </table>
                        </td>
                      </tr>
                    )}
                  </Fragment>
                );
              })}
            </tbody>
          </table>
        </div>
      )}
    </section>
  );
}
