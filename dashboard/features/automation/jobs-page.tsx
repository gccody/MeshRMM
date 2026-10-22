import { CircleAlert, ListChecks, LoaderCircle, RefreshCw, X } from "lucide-react";
import { useCallback } from "react";
import { Link, useSearchParams } from "react-router";
import { formatDateTime } from "../../lib/format";
import { useResource } from "../../lib/use-resource";
import { HeaderActions } from "../workspace/header-actions";
import { useWorkspace } from "../workspace/workspace-context";
import { AutomationAreaTabs } from "./area-tabs";
import { fetchJobs } from "./automation-api";
import { JobProgressBar } from "./job-progress";
import { type Job, applyJobUpdate, countsSummary, isJobFinished, jobStarter } from "./model";
import { useJobEvents } from "./use-job-feed";

// `/jobs`: recent jobs, newest first, or one policy's with `?policy=`. The
// event socket keeps their progress current.
export function JobsPage() {
  const { authorizedFetch } = useWorkspace();
  const [searchParams, setSearchParams] = useSearchParams();
  const policyId = searchParams.get("policy");
  const load = useCallback(() => fetchJobs(authorizedFetch, policyId), [authorizedFetch, policyId]);
  const { data: jobs, error, reload, setData } = useResource(load, "The jobs could not be loaded.");

  useJobEvents((event) => {
    if (event.type === "jobs_resync") reload();
    else setData((current) => applyJobUpdate(current, event.job, policyId));
  });

  const policyName = policyId ? jobs?.find((job) => job.policy_id === policyId)?.policy_name : null;

  return (
    <>
      <HeaderActions>
        <button type="button" className="icon-button" onClick={reload} aria-label="Refresh" title="Refresh"><RefreshCw size={16} /></button>
      </HeaderActions>
      <AutomationAreaTabs current="jobs" />
      {policyId && (
        <div className="filter-notice" role="status">
          <span>Showing the jobs of {policyName ? `the policy “${policyName}”` : "one policy"}.</span>
          <button type="button" className="link-button" onClick={() => setSearchParams(new URLSearchParams(), { replace: true })}><X size={14} aria-hidden="true" /> Show all jobs</button>
        </div>
      )}
      <section className="agent-panel">
        {!jobs ? (
          error
            ? <div className="empty-state"><CircleAlert size={22} aria-hidden="true" /><strong>{error}</strong><button type="button" className="secondary-button" onClick={reload}><RefreshCw size={16} /> Try again</button></div>
            : <div className="empty-state"><LoaderCircle size={22} className="spin" aria-hidden="true" /><strong>Loading jobs…</strong></div>
        ) : jobs.length === 0 ? (
          <div className="empty-state">
            <ListChecks size={22} aria-hidden="true" />
            <strong>No jobs yet</strong>
            <span>Run a script or send a file on several devices from the Toolbox or the Devices page, or let a policy start one. Jobs are kept for 30 days.</span>
          </div>
        ) : (
          <>
            {error && <p className="table-message form-error" role="alert"><CircleAlert size={16} aria-hidden="true" />{error}</p>}
            <JobTable jobs={jobs} />
          </>
        )}
      </section>
    </>
  );
}

function JobTable({ jobs }: { jobs: Job[] }) {
  return (
    <table className="data-table job-table" aria-label="Jobs">
      <thead>
        <tr>
          <th scope="col">Job</th>
          <th scope="col">Devices</th>
          <th scope="col">Started by</th>
          <th scope="col">Progress</th>
          <th scope="col">Started</th>
        </tr>
      </thead>
      <tbody>
        {jobs.map((job) => {
          const finished = isJobFinished(job);
          return (
            <tr key={job.id}>
              <td>
                <strong><Link to={`/job?id=${encodeURIComponent(job.id)}`}>{job.item_name}</Link></strong>
                <span>{job.kind === "script" ? "Script" : "File"}{finished ? "" : " · Running"}</span>
              </td>
              <td>{job.target_label}<span>{job.counts.total} {job.counts.total === 1 ? "device" : "devices"}</span></td>
              <td>{jobStarter(job)}</td>
              <td className="job-progress-cell"><JobProgressBar counts={job.counts} /><span>{countsSummary(job.counts)}</span></td>
              <td className="nowrap">{formatDateTime(job.created_at_unix_ms)}</td>
            </tr>
          );
        })}
      </tbody>
    </table>
  );
}
