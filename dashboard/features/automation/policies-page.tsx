import { CalendarClock, CircleAlert, LoaderCircle, Pencil, Play, Plus, RefreshCw, Trash2, TriangleAlert, X } from "lucide-react";
import { useCallback, useMemo, useRef, useState } from "react";
import { Link, useNavigate } from "react-router";
import { formatDateTime } from "../../lib/format";
import { AuthenticationRequired, type AuthorizedFetch, RequestError, errorText } from "../../lib/http";
import { useResource } from "../../lib/use-resource";
import { fetchChannels } from "../alerts/alerts-api";
import type { Channel } from "../alerts/model";
import type { ToolboxScript } from "../toolbox/model";
import { fetchToolbox } from "../toolbox/toolbox-api";
import { HeaderActions } from "../workspace/header-actions";
import { useWorkspace } from "../workspace/workspace-context";
import { AutomationAreaTabs } from "./area-tabs";
import { deletePolicy, fetchPolicies, runPolicy, savePolicy } from "./automation-api";
import { JobProgressBar } from "./job-progress";
import { type Policy, SAVE_PERMISSION, countsSummary, describePolicyAction, describeSchedule, describeTarget, isJobFinished, policyDraft, policyInput } from "./model";
import { PolicyEditor } from "./policy-editor";

const byName = (left: Policy, right: Policy) => left.name.localeCompare(right.name, undefined, { sensitivity: "base" });

// The notification channels, or null when the user may not see them.
async function channelsOrNull(authorizedFetch: AuthorizedFetch): Promise<Channel[] | null> {
  try {
    return (await fetchChannels(authorizedFetch)).channels;
  } catch (error) {
    if (error instanceof RequestError && error.status === 403) return null;
    throw error;
  }
}

// The shared scripts, or none when the user may not see the toolbox.
async function sharedScripts(authorizedFetch: AuthorizedFetch): Promise<ToolboxScript[]> {
  try {
    return (await fetchToolbox(authorizedFetch)).scripts.filter((script) => script.shared);
  } catch (error) {
    if (error instanceof RequestError && error.status === 403) return [];
    throw error;
  }
}

// `/policies`: scripts the server runs by itself, and updates it has devices
// scan for and install, on a schedule or on newly enrolled devices.
export function PoliciesPage() {
  const { authorizedFetch, inventory, can } = useWorkspace();
  const navigate = useNavigate();
  // A saved policy runs with its saver's access, which must allow what it does.
  const canSaveAny = Object.values(SAVE_PERMISSION).some((permission) => can(permission));
  const canSave = (policy: Policy) => can(SAVE_PERMISSION[policy.action]);
  const [dialog, setDialog] = useState<{ policy: Policy | null } | null>(null);
  const [busyId, setBusyId] = useState<string | null>(null);
  const [actionError, setActionError] = useState<string | null>(null);
  const opener = useRef<HTMLElement | null>(null);

  const load = useCallback(async () => {
    const [policies, scripts, channels] = await Promise.all([
      fetchPolicies(authorizedFetch),
      sharedScripts(authorizedFetch),
      channelsOrNull(authorizedFetch),
    ]);
    return { policies: policies.sort(byName), scripts, channels };
  }, [authorizedFetch]);
  const { data, error, reload, setData } = useResource(load, "The policies could not be loaded.");
  const companyNames = useMemo(() => new Map(inventory.companies.map((company) => [company.id, company.name])), [inventory.companies]);
  const deviceNames = useMemo(() => new Map(inventory.agents.map((agent) => [agent.id, agent.name])), [inventory.agents]);

  const replace = (saved: Policy) => setData((current) => ({
    ...current,
    policies: [...current.policies.filter((candidate) => candidate.id !== saved.id), { ...saved, last_job: current.policies.find((candidate) => candidate.id === saved.id)?.last_job ?? null }].sort(byName),
  }));

  const act = async (policy: Policy, action: () => Promise<void>, fallback: string) => {
    setBusyId(policy.id);
    setActionError(null);
    try {
      await action();
    } catch (failure) {
      if (!(failure instanceof AuthenticationRequired)) setActionError(errorText(failure, fallback));
    } finally {
      setBusyId(null);
    }
  };

  const toggle = (policy: Policy) => act(policy, async () => {
    replace(await savePolicy(authorizedFetch, policy.id, policyInput({ ...policyDraft(policy, policy.time_zone ?? "UTC"), enabled: !policy.enabled })));
  }, policy.enabled ? "The policy could not be turned off." : "The policy could not be turned on.");

  const run = (policy: Policy) => act(policy, async () => {
    const job = await runPolicy(authorizedFetch, policy.id);
    void navigate(`/job?id=${encodeURIComponent(job.id)}`);
  }, "The policy could not be run.");

  const remove = (policy: Policy) => {
    if (!window.confirm(`Delete the policy “${policy.name}”? Its jobs stay in the history.`)) return;
    void act(policy, async () => {
      await deletePolicy(authorizedFetch, policy.id);
      setData((current) => ({ ...current, policies: current.policies.filter((candidate) => candidate.id !== policy.id) }));
    }, "The policy could not be deleted.");
  };

  const open = (policy: Policy | null, button: HTMLElement) => {
    opener.current = button;
    setDialog({ policy });
  };

  return (
    <>
      <HeaderActions>
        <button type="button" className="icon-button" onClick={reload} aria-label="Refresh" title="Refresh"><RefreshCw size={16} /></button>
        {data && canSaveAny && <button type="button" className="primary-button" onClick={(event) => open(null, event.currentTarget)} aria-haspopup="dialog"><Plus size={16} /> New policy</button>}
      </HeaderActions>
      <AutomationAreaTabs current="policies" />
      {actionError && (
        <div className="error-banner" role="alert">
          <CircleAlert size={17} aria-hidden="true" /><span>{actionError}</span>
          <button onClick={() => setActionError(null)} aria-label="Dismiss"><X size={16} /></button>
        </div>
      )}
      <section className="agent-panel">
        {!data ? (
          error
            ? <div className="empty-state"><CircleAlert size={22} aria-hidden="true" /><strong>{error}</strong><button type="button" className="secondary-button" onClick={reload}><RefreshCw size={16} /> Try again</button></div>
            : <div className="empty-state"><LoaderCircle size={22} className="spin" aria-hidden="true" /><strong>Loading policies…</strong></div>
        ) : data.policies.length === 0 ? (
          <div className="empty-state">
            <CalendarClock size={22} aria-hidden="true" />
            <strong>No policies yet</strong>
            <span>A policy runs a shared script, scans for or installs operating system updates, or wakes offline devices, on a schedule or on each device that enrolls, and keeps its runs on the Jobs tab.{canSaveAny ? "" : " Creating one needs the scripts.run, updates.view, updates.install or devices.wake permission too."}</span>
          </div>
        ) : (
          <table className="data-table policy-table" aria-label="Policies">
            <thead>
              <tr>
                <th scope="col">Policy</th>
                <th scope="col">Runs</th>
                <th scope="col">Last run</th>
                <th scope="col">On</th>
                <th scope="col"><span className="sr-only">Actions</span></th>
              </tr>
            </thead>
            <tbody>
              {data.policies.map((policy) => {
                const busy = busyId === policy.id;
                return (
                  <tr key={policy.id} className={policy.enabled ? undefined : "row-disabled"}>
                    <td>
                      <strong>{policy.name}</strong>
                      <span>{describePolicyAction(policy)} on {describeTarget(policy, companyNames, deviceNames)}</span>
                      <span>Runs with {policy.owned_by_you ? "your" : `${policy.owner}’s`} access</span>
                    </td>
                    <td>
                      {describeSchedule(policy)}
                      <span>{!policy.enabled ? "Off" : policy.next_run_at ? `Next ${formatDateTime(policy.next_run_at)}` : policy.trigger === "enroll" ? "On each new device" : "Nothing left to run"}</span>
                    </td>
                    <td>
                      {policy.last_job ? (
                        <>
                          <Link to={`/job?id=${encodeURIComponent(policy.last_job.id)}`}>{formatDateTime(policy.last_job.created_at_unix_ms)}</Link>
                          <span className="policy-last-job"><JobProgressBar counts={policy.last_job.counts} />{isJobFinished(policy.last_job) ? countsSummary(policy.last_job.counts) : "Running"}</span>
                          <span><Link to={`/jobs?policy=${encodeURIComponent(policy.id)}`}>All runs</Link></span>
                        </>
                      ) : policy.last_run_at ? formatDateTime(policy.last_run_at) : <span>Not yet</span>}
                      {policy.last_problem && <span className="policy-problem"><TriangleAlert size={14} aria-hidden="true" />{policy.last_problem}</span>}
                    </td>
                    <td>
                      <label className="switch">
                        <input type="checkbox" role="switch" checked={policy.enabled} disabled={busy || !canSave(policy)} onChange={() => void toggle(policy)} aria-label={`${policy.name} turned on`} />
                        <span aria-hidden="true" />
                      </label>
                    </td>
                    <td className="row-actions">
                      {canSave(policy) && <>
                        <button type="button" className="close-session-button" disabled={busy} onClick={() => void run(policy)} aria-label={`Run ${policy.name} now`} title="Run now">{busy ? <LoaderCircle size={16} className="spin" /> : <Play size={16} />}</button>
                        <button type="button" className="close-session-button" onClick={(event) => open(policy, event.currentTarget)} aria-haspopup="dialog" aria-label={`Edit ${policy.name}`} title="Edit policy"><Pencil size={16} /></button>
                      </>}
                      <button type="button" className="agent-delete-button" disabled={busy} onClick={() => remove(policy)} aria-label={`Delete ${policy.name}`} title="Delete policy"><Trash2 size={16} /></button>
                    </td>
                  </tr>
                );
              })}
            </tbody>
          </table>
        )}
      </section>
      {dialog && data && (
        <PolicyEditor
          policy={dialog.policy}
          scripts={data.scripts}
          channels={data.channels}
          returnFocus={opener}
          onClose={() => setDialog(null)}
          onSaved={(saved) => {
            replace(saved);
            setDialog(null);
          }}
        />
      )}
    </>
  );
}
