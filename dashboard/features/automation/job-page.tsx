import { Ban, CircleAlert, LoaderCircle, RefreshCw, SearchX, X } from "lucide-react";
import { type RefObject, useCallback, useEffect, useMemo, useRef, useState } from "react";
import { Link, useSearchParams } from "react-router";
import { formatDateTime } from "../../lib/format";
import { AuthenticationRequired, errorText } from "../../lib/http";
import { ModalDialog } from "../../lib/modal-dialog";
import { useResource } from "../../lib/use-resource";
import { PLATFORM_LABELS } from "../agents/types";
import { LANGUAGE_LABELS, RUN_AS_LABELS } from "../toolbox/model";
import { describeReboot, describeSelection } from "../updates/model";
import { RunResult, useScriptRun } from "../toolbox/run-result";
import { fetchRun } from "../toolbox/toolbox-api";
import { HeaderActions } from "../workspace/header-actions";
import { useWorkspace } from "../workspace/workspace-context";
import { AutomationAreaTabs } from "./area-tabs";
import { cancelJob, fetchJob } from "./automation-api";
import { JobProgressBar, OutcomeChips, OutcomePill } from "./job-progress";
import { type JobDetail, type JobDevice, type Outcome, countsSummary, deviceResult, isJobFinished, isUpdateJob, jobStarter, progressLabel } from "./model";
import { useJobEvents } from "./use-job-feed";

// Unfinished jobs are read again this often, in case an event was missed.
const POLL_INTERVAL_MS = 10_000;
// Events arrive as each device reports; the devices are read at most this often.
const EVENT_DEBOUNCE_MS = 1_000;

const DESTINATION_LABELS = {
  user: "The signed-in user's Documents",
  public: "Public Documents",
} as const;

// `/job?id=`: what a job sends, to which devices, and how each one went.
export function JobPage() {
  const [searchParams] = useSearchParams();
  const id = searchParams.get("id") ?? "";
  if (!id) {
    return (
      <section className="agent-panel">
        <div className="empty-state"><SearchX size={22} aria-hidden="true" /><strong>No job chosen</strong><Link className="secondary-button" to="/jobs">See all jobs</Link></div>
      </section>
    );
  }
  return <JobView key={id} id={id} />;
}

function JobView({ id }: { id: string }) {
  const { authorizedFetch, setPageTitle } = useWorkspace();
  const load = useCallback(() => fetchJob(authorizedFetch, id), [authorizedFetch, id]);
  const { data: job, error, reload, setData } = useResource(load, "The job could not be loaded.");
  const [filter, setFilter] = useState<Outcome | null>(null);
  const [output, setOutput] = useState<JobDevice | null>(null);
  const [cancelling, setCancelling] = useState(false);
  const [actionError, setActionError] = useState<string | null>(null);
  const opener = useRef<HTMLElement | null>(null);
  const pending = useRef<number | undefined>(undefined);
  const finished = job ? isJobFinished(job) : false;

  useEffect(() => {
    setPageTitle(job?.item_name ?? null);
    return () => setPageTitle(null);
  }, [job?.item_name, setPageTitle]);

  useEffect(() => () => window.clearTimeout(pending.current), []);

  useJobEvents((event) => {
    if (event.type === "job_update" && event.job.id !== id) return;
    // The counts are current at once; the devices follow.
    if (event.type === "job_update") setData((current) => ({ ...current, ...event.job }));
    if (pending.current !== undefined) return;
    pending.current = window.setTimeout(() => {
      pending.current = undefined;
      reload();
    }, EVENT_DEBOUNCE_MS);
  });

  useEffect(() => {
    if (!job || finished) return;
    const timer = window.setInterval(reload, POLL_INTERVAL_MS);
    return () => window.clearInterval(timer);
  }, [job, finished, reload]);

  const devices = useMemo(() => (job?.devices ?? []).filter((device) => filter === null || device.outcome === filter), [job, filter]);

  const cancel = async () => {
    if (!job || !window.confirm("Cancel the devices that haven't started? Scripts and installs already running finish.")) return;
    setCancelling(true);
    setActionError(null);
    try {
      const next = await cancelJob(authorizedFetch, job.id);
      setData(() => next);
    } catch (failure) {
      if (!(failure instanceof AuthenticationRequired)) setActionError(errorText(failure, "The job could not be cancelled."));
    } finally {
      setCancelling(false);
    }
  };

  if (!job) {
    return (
      <>
        <AutomationAreaTabs current="job" />
        <section className="agent-panel">
          {error
            ? <div className="empty-state"><CircleAlert size={22} aria-hidden="true" /><strong>{error}</strong><button type="button" className="secondary-button" onClick={reload}><RefreshCw size={16} /> Try again</button></div>
            : <div className="empty-state"><LoaderCircle size={22} className="spin" aria-hidden="true" /><strong>Loading the job…</strong></div>}
        </section>
      </>
    );
  }

  return (
    <>
      <HeaderActions>
        {job.can_cancel && <button type="button" className="danger-button" onClick={() => void cancel()} disabled={cancelling}>{cancelling ? <LoaderCircle size={16} className="spin" /> : <Ban size={16} />} Cancel the rest</button>}
        <button type="button" className="icon-button" onClick={reload} aria-label="Refresh" title="Refresh"><RefreshCw size={16} /></button>
      </HeaderActions>
      <AutomationAreaTabs current="job" />
      {(actionError || (error && job)) && (
        <div className="error-banner" role="alert">
          <CircleAlert size={17} aria-hidden="true" /><span>{actionError ?? error}</span>
          {actionError && <button onClick={() => setActionError(null)} aria-label="Dismiss"><X size={16} /></button>}
        </div>
      )}
      <section className="agent-panel job-summary">
        <JobFacts job={job} />
        <div className="job-summary-progress">
          <JobProgressBar counts={job.counts} />
          <span>{finished ? `Finished: ${countsSummary(job.counts)}` : progressLabel(job.counts)}</span>
        </div>
        <OutcomeChips counts={job.counts} kind={job.kind} selected={filter} onSelect={setFilter} />
      </section>
      <section className="agent-panel">
        {devices.length === 0 ? (
          <div className="empty-state"><SearchX size={22} aria-hidden="true" /><strong>No devices to show</strong>{filter && <button type="button" className="secondary-button" onClick={() => setFilter(null)}>Show every device</button>}</div>
        ) : (
          <table className="data-table" aria-label="Devices in the job">
            <thead>
              <tr>
                <th scope="col">Device</th>
                <th scope="col">Result</th>
                <th scope="col">Finished</th>
                <th scope="col"><span className="sr-only">Output</span></th>
              </tr>
            </thead>
            <tbody>
              {devices.map((device) => (
                <tr key={device.device_id}>
                  <td>
                    <strong>{device.device_name ? <Link to={`/device?id=${encodeURIComponent(device.device_id)}`}>{device.device_name}</Link> : "Deleted device"}</strong>
                    <span>{device.platform ? PLATFORM_LABELS[device.platform] : ""}</span>
                  </td>
                  <td><OutcomePill outcome={device.outcome} kind={job.kind} />{deviceResult(device) && <span>{deviceResult(device)}</span>}</td>
                  <td className="nowrap">{device.completed_at_unix_ms ? formatDateTime(device.completed_at_unix_ms) : ""}</td>
                  <td className="row-actions">
                    {device.run_id && <button type="button" className="secondary-button" onClick={(event) => { opener.current = event.currentTarget; setOutput(device); }} aria-haspopup="dialog">View output</button>}
                    {device.update_run_id && device.device_name && <Link className="secondary-button" to={`/device?${new URLSearchParams({ id: device.device_id, tab: "updates" })}`}>View updates</Link>}
                  </td>
                </tr>
              ))}
            </tbody>
          </table>
        )}
      </section>
      {output?.run_id && <RunOutputModal runId={output.run_id} deviceName={output.device_name ?? "Deleted device"} returnFocus={opener} onClose={() => setOutput(null)} />}
    </>
  );
}

// What the job sends, where, for whom, and what happens to offline devices.
function JobFacts({ job }: { job: JobDetail }) {
  return (
    <dl className="job-facts">
      {job.kind === "wake"
        ? <div><dt>Action</dt><dd>{job.item_name}<span>Offline devices are woken through online devices on their network</span></dd></div>
        : isUpdateJob(job.kind)
        ? <div><dt>Updates</dt><dd>{job.item_name}{job.kind === "update_install" && <span>{describeSelection({ categories: job.categories ?? [] })}</span>}</dd></div>
        : <div><dt>{job.kind === "script" ? "Script" : "File"}</dt><dd>{job.item_name}{job.language && <span>{LANGUAGE_LABELS[job.language]}</span>}</dd></div>}
      {job.kind === "update_install" && <div><dt>Restart</dt><dd>{describeReboot(job.reboot ?? null, job.reboot_delay_minutes ?? null)}</dd></div>}
      {job.run_as && <div><dt>Runs as</dt><dd>{RUN_AS_LABELS[job.run_as]}</dd></div>}
      {job.destination && <div><dt>Saved to</dt><dd>{DESTINATION_LABELS[job.destination]}</dd></div>}
      <div><dt>On</dt><dd>{job.target_label}</dd></div>
      <div><dt>Started by</dt><dd>{job.policy_id ? <Link to={`/jobs?policy=${encodeURIComponent(job.policy_id)}`}>{jobStarter(job)}</Link> : jobStarter(job)}{job.policy_id && <span>With {job.requested_by_you ? "your" : `${job.requested_by}’s`} access</span>}</dd></div>
      <div><dt>Started</dt><dd>{formatDateTime(job.created_at_unix_ms)}</dd></div>
      {job.kind === "wake"
        ? <div><dt>Online devices</dt><dd>Skipped</dd></div>
        : <div><dt>Offline devices</dt><dd>{job.offline === "wait" ? `Run if they connect by ${formatDateTime(job.expires_at_unix_ms)}` : "Skipped"}</dd></div>}
      {job.finished_at_unix_ms !== null && <div><dt>Finished</dt><dd>{formatDateTime(job.finished_at_unix_ms)}</dd></div>}
    </dl>
  );
}

// A device's run of the job's script, with its output.
function RunOutputModal({ runId, deviceName, returnFocus, onClose }: {
  runId: string;
  deviceName: string;
  returnFocus: RefObject<HTMLElement | null>;
  onClose: () => void;
}) {
  const { authorizedFetch } = useWorkspace();
  const load = useCallback(() => fetchRun(authorizedFetch, runId), [authorizedFetch, runId]);
  const { data: loaded, error: loadError } = useResource(load, "The run could not be loaded.");
  const { run, error } = useScriptRun(authorizedFetch, loaded);
  return (
    <ModalDialog className="settings-modal toolbox-modal" labelledBy="job-run-title" onClose={onClose} returnFocus={returnFocus}>
      <button type="button" className="modal-close" onClick={onClose} aria-label="Close"><X size={19} /></button>
      <h2 id="job-run-title">{deviceName}</h2>
      {run
        ? <RunResult run={run} deviceName={deviceName} error={error} />
        : loadError ? <p role="alert" className="installer-error">{loadError}</p> : <p className="page-status" role="status"><LoaderCircle size={16} className="spin" /> Loading the output…</p>}
      <div className="connection-reason-actions"><button type="button" className="primary-button" onClick={onClose}>Done</button></div>
    </ModalDialog>
  );
}
