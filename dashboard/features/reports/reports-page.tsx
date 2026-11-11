import { CalendarClock, CircleAlert, Download, ExternalLink, FileBarChart, LoaderCircle, Pencil, Play, Plus, RefreshCw, Trash2, X } from "lucide-react";
import { useCallback, useEffect, useRef, useState } from "react";
import { useSearchParams } from "react-router";
import { AuthenticationRequired, errorText } from "../../lib/http";
import { formatDateTime, formatRelative } from "../../lib/format";
import { useResource } from "../../lib/use-resource";
import { useCompanies, useTags } from "../agents/use-devices";
import { HeaderActions } from "../workspace/header-actions";
import { useWorkspace } from "../workspace/workspace-context";
import {
  DELIVERY_LABELS,
  type Report,
  STATUS_LABELS,
  type Schedule,
  type TemplateId,
  type Templates,
  describeFrequency,
  describeRequester,
  describeScope,
  formatSize,
  isWorking,
  reportDownload,
  reportPage,
  tableDownload,
} from "./model";
import { deleteReport, deleteSchedule, fetchReport, fetchReports, fetchSchedules, fetchTemplates, runSchedule } from "./reports-api";
import { RunDialog } from "./run-dialog";
import { ScheduleEditor } from "./schedule-editor";

// How often the list is read again while a report is being made or sent.
const POLL_MS = 2000;

type Tab = "reports" | "schedules";

// `/reports`: the report templates, the reports made from them, and the
// schedules that make them.
export function ReportsPage() {
  const { authorizedFetch, can, inventory } = useWorkspace();
  const companies = useCompanies(inventory.devices);
  const tags = useTags(inventory.devices);
  const manage = can("reports.manage");
  const [params, setParams] = useSearchParams();
  const [tab, setTab] = useState<Tab>(params.get("tab") === "schedules" ? "schedules" : "reports");
  const [running, setRunning] = useState<TemplateId | null>(null);
  const [editing, setEditing] = useState<{ schedule: Schedule | null } | null>(null);
  const [busyId, setBusyId] = useState<string | null>(null);
  const [actionError, setActionError] = useState<string | null>(null);
  const [opened, setOpened] = useState<Report | null>(null);
  const opener = useRef<HTMLElement | null>(null);

  const load = useCallback(async () => {
    const [templates, list, schedules] = await Promise.all([fetchTemplates(authorizedFetch), fetchReports(authorizedFetch), fetchSchedules(authorizedFetch)]);
    return { templates, reports: list.reports, nextBefore: list.next_before, schedules: schedules.schedules };
  }, [authorizedFetch]);
  const { data, error, reload, setData } = useResource(load, "The reports could not be loaded.");

  // Follows reports being made until they're done.
  const working = data ? isWorking(data.reports) : false;
  useEffect(() => {
    if (!working) return;
    const timer = window.setTimeout(() => {
      fetchReports(authorizedFetch).then(
        (list) => setData((current) => ({ ...current, reports: mergeReports(list.reports, current.reports) })),
        () => undefined,
      );
    }, POLL_MS);
    return () => window.clearTimeout(timer);
  }, [authorizedFetch, data, setData, working]);

  // A link to one report, as its email carries, shows its tables.
  const linked = params.get("report");
  useEffect(() => {
    if (!linked) return;
    let cancelled = false;
    fetchReport(authorizedFetch, linked).then(
      (report) => { if (!cancelled) setOpened(report); },
      (failure: unknown) => { if (!cancelled && !(failure instanceof AuthenticationRequired)) setActionError(errorText(failure, "The report could not be loaded.")); },
    );
    return () => { cancelled = true; };
  }, [authorizedFetch, linked]);

  const choose = (next: Tab) => {
    setTab(next);
    setParams(next === "schedules" ? { tab: next } : {}, { replace: true });
  };

  const queued = (report: Report) => {
    setRunning(null);
    setData((current) => ({ ...current, reports: [report, ...current.reports.filter((candidate) => candidate.id !== report.id)] }));
    choose("reports");
  };

  const act = async (id: string, fallback: string, action: () => Promise<void>) => {
    setBusyId(id);
    setActionError(null);
    try {
      await action();
    } catch (failure) {
      if (!(failure instanceof AuthenticationRequired)) setActionError(errorText(failure, fallback));
    } finally {
      setBusyId(null);
    }
  };

  const removeReport = (report: Report) => {
    if (!window.confirm(`Delete this ${report.title ?? "report"}? Its page and downloads go too.`)) return;
    void act(report.id, "The report could not be deleted.", async () => {
      await deleteReport(authorizedFetch, report.id);
      setData((current) => ({ ...current, reports: current.reports.filter((candidate) => candidate.id !== report.id) }));
      if (opened?.id === report.id) setOpened(null);
    });
  };

  const removeSchedule = (schedule: Schedule) => {
    if (!window.confirm(`Delete the schedule “${schedule.name}”? The reports it made are kept until they expire.`)) return;
    void act(schedule.id, "The schedule could not be deleted.", async () => {
      await deleteSchedule(authorizedFetch, schedule.id);
      setData((current) => ({ ...current, schedules: current.schedules.filter((candidate) => candidate.id !== schedule.id) }));
    });
  };

  const runNow = (schedule: Schedule) => void act(schedule.id, "The schedule could not be run.", async () => {
    queued(await runSchedule(authorizedFetch, schedule.id));
  });

  const showMore = () => {
    if (!data?.nextBefore) return;
    void act("more", "More reports could not be loaded.", async () => {
      const list = await fetchReports(authorizedFetch, data.nextBefore);
      setData((current) => ({ ...current, reports: [...current.reports, ...list.reports], nextBefore: list.next_before }));
    });
  };

  const openEditor = (schedule: Schedule | null, button: HTMLElement) => {
    opener.current = button;
    setEditing({ schedule });
  };

  return (
    <>
      <HeaderActions>
        {data && manage && tab === "schedules" && <button type="button" className="primary-button" onClick={(event) => openEditor(null, event.currentTarget)} aria-haspopup="dialog"><Plus size={16} /> New schedule</button>}
      </HeaderActions>
      <div className="toolbox-tabs area-tabs" role="tablist" aria-label="Reports">
        <button type="button" role="tab" aria-selected={tab === "reports"} onClick={() => choose("reports")}><FileBarChart size={16} aria-hidden="true" />Reports</button>
        <button type="button" role="tab" aria-selected={tab === "schedules"} onClick={() => choose("schedules")}><CalendarClock size={16} aria-hidden="true" />Schedules{data ? <em>{data.schedules.length}</em> : null}</button>
      </div>
      {actionError && (
        <div className="error-banner" role="alert">
          <CircleAlert size={17} aria-hidden="true" /><span>{actionError}</span>
          <button onClick={() => setActionError(null)} aria-label="Dismiss"><X size={16} /></button>
        </div>
      )}
      {data && !data.templates.email_configured && (
        <p className="auth-note page-notice">Email isn&apos;t set up on this server, so schedules can&apos;t email their reports. They still make them, and keep them here. An administrator can set up email under Authentication, in Email.</p>
      )}
      {!data ? (
        <section className="agent-panel">
          {error
            ? <div className="empty-state"><CircleAlert size={22} aria-hidden="true" /><strong>{error}</strong><button type="button" className="secondary-button" onClick={reload}><RefreshCw size={16} /> Try again</button></div>
            : <div className="empty-state"><LoaderCircle size={22} className="spin" aria-hidden="true" /><strong>Loading reports…</strong></div>}
        </section>
      ) : tab === "reports" ? (
        <>
          <TemplateCards templates={data.templates} onRun={(template, button) => { opener.current = button; setRunning(template); }} />
          {opened && <OpenedReport report={opened} onClose={() => { setOpened(null); setParams({}, { replace: true }); }} />}
          <section className="agent-panel">
            {data.reports.length === 0 ? (
              <div className="empty-state">
                <FileBarChart size={22} aria-hidden="true" />
                <strong>No reports yet</strong>
                <span>Run one of the reports above, or schedule one. Reports are kept for {data.templates.retention_days} days.</span>
              </div>
            ) : (
              <table className="data-table" aria-label="Generated reports">
                <thead>
                  <tr>
                    <th scope="col">Report</th>
                    <th scope="col">Covers</th>
                    <th scope="col">Made</th>
                    <th scope="col">Status</th>
                    <th scope="col"><span className="sr-only">Actions</span></th>
                  </tr>
                </thead>
                <tbody>
                  {data.reports.map((report) => (
                    <tr key={report.id}>
                      <td><strong>{report.title ?? report.template}</strong><span>{describeRequester(report)}</span></td>
                      <td><span>{report.scope_label}</span><span>{report.period_label}</span></td>
                      <td><span title={formatDateTime(report.created_at)}>{formatRelative(report.created_at)}</span><span>Kept until {formatDateTime(report.expires_at)}</span></td>
                      <td><ReportStatus report={report} /></td>
                      <td className="row-actions">
                        {report.status === "succeeded" && <>
                          <a className="secondary-button" href={reportPage(report.id)} target="_blank" rel="noreferrer"><ExternalLink size={15} aria-hidden="true" /> Open</a>
                          <a className="close-session-button" href={reportDownload(report.id)} aria-label={`Download ${report.title ?? "report"} as HTML`} title={`Download (${formatSize(report.size_bytes)})`}><Download size={16} /></a>
                        </>}
                        {manage && <button type="button" className="agent-delete-button" disabled={busyId === report.id} onClick={() => removeReport(report)} aria-label={`Delete ${report.title ?? "report"}`} title="Delete report"><Trash2 size={16} /></button>}
                      </td>
                    </tr>
                  ))}
                </tbody>
              </table>
            )}
          </section>
          {data.nextBefore !== null && <button type="button" className="secondary-button" disabled={busyId === "more"} onClick={showMore}>Show older reports</button>}
        </>
      ) : (
        <section className="agent-panel">
          {data.schedules.length === 0 ? (
            <div className="empty-state">
              <CalendarClock size={22} aria-hidden="true" />
              <strong>No schedules yet</strong>
              <span>{manage ? "Make a report daily, weekly or monthly, and email it to chosen users and addresses." : "Someone with the reports.manage permission can schedule reports."}</span>
            </div>
          ) : (
            <table className="data-table" aria-label="Report schedules">
              <thead>
                <tr>
                  <th scope="col">Schedule</th>
                  <th scope="col">Runs</th>
                  <th scope="col">Emails</th>
                  <th scope="col">Last report</th>
                  <th scope="col"><span className="sr-only">Actions</span></th>
                </tr>
              </thead>
              <tbody>
                {data.schedules.map((schedule) => {
                  const template = data.templates.templates.find((candidate) => candidate.id === schedule.template);
                  const recipients = schedule.user_ids.length + schedule.emails.length;
                  return (
                    <tr key={schedule.id} className={schedule.enabled ? undefined : "row-disabled"}>
                      <td>
                        <strong>{schedule.name}{!schedule.enabled && <span className="badge badge-muted">Off</span>}</strong>
                        <span>{template?.title ?? schedule.template} · {describeScope(schedule.target, schedule.company_ids, schedule.tag_ids, schedule.tag_match, companies, tags)}</span>
                      </td>
                      <td>
                        <span>{describeFrequency(schedule)}</span>
                        {schedule.next_run_at !== null && <span title={formatDateTime(schedule.next_run_at)}>Next {formatDateTime(schedule.next_run_at)}</span>}
                      </td>
                      <td><span>{recipients === 0 ? "Nobody" : recipients === 1 ? "1 recipient" : `${recipients} recipients`}</span><span>Runs as {schedule.owned_by_you ? "you" : schedule.owner}</span></td>
                      <td>{schedule.last_run_at === null ? <span>Never</span> : <span title={formatDateTime(schedule.last_run_at)}>{formatRelative(schedule.last_run_at)}</span>}</td>
                      <td className="row-actions">
                        {manage && <>
                          <button type="button" className="secondary-button" disabled={busyId === schedule.id} onClick={() => runNow(schedule)}>{busyId === schedule.id ? <LoaderCircle size={15} className="spin" /> : <Play size={15} />} Run now</button>
                          <button type="button" className="close-session-button" onClick={(event) => openEditor(schedule, event.currentTarget)} aria-haspopup="dialog" aria-label={`Edit ${schedule.name}`} title="Edit schedule"><Pencil size={16} /></button>
                          <button type="button" className="agent-delete-button" disabled={busyId === schedule.id} onClick={() => removeSchedule(schedule)} aria-label={`Delete ${schedule.name}`} title="Delete schedule"><Trash2 size={16} /></button>
                        </>}
                      </td>
                    </tr>
                  );
                })}
              </tbody>
            </table>
          )}
        </section>
      )}
      {running && data && <RunDialog templates={data.templates} template={running} returnFocus={opener} onClose={() => setRunning(null)} onQueued={queued} />}
      {editing && data && (
        <ScheduleEditor
          schedule={editing.schedule}
          templates={data.templates}
          returnFocus={opener}
          onClose={() => setEditing(null)}
          onSaved={(saved) => {
            setData((current) => ({ ...current, schedules: [...current.schedules.filter((candidate) => candidate.id !== saved.id), saved].sort((left, right) => left.name.localeCompare(right.name)) }));
            setEditing(null);
          }}
        />
      )}
    </>
  );
}

// Newer pages of the list replace the reports they hold; older ones stay.
function mergeReports(fresh: readonly Report[], current: readonly Report[]) {
  const ids = new Set(fresh.map((report) => report.id));
  const oldest = fresh.at(-1)?.created_at ?? Infinity;
  return [...fresh, ...current.filter((report) => !ids.has(report.id) && report.created_at < oldest)];
}

function TemplateCards({ templates, onRun }: { templates: Templates; onRun: (template: TemplateId, button: HTMLElement) => void }) {
  return (
    <ul className="report-templates" aria-label="Report templates">
      {templates.templates.map((template) => (
        <li key={template.id}>
          <strong>{template.title}</strong>
          <span>{template.description}</span>
          <button type="button" className="secondary-button" onClick={(event) => onRun(template.id, event.currentTarget)} aria-haspopup="dialog"><Play size={15} /> Run</button>
        </li>
      ))}
    </ul>
  );
}

function ReportStatus({ report }: { report: Report }) {
  const tone = report.status === "succeeded" ? "run-pill-success" : report.status === "failed" ? "run-pill-problem" : "run-pill-pending";
  return (
    <>
      <span className={`run-pill ${tone}`}>{(report.status === "queued" || report.status === "running") && <LoaderCircle size={12} className="spin" aria-hidden="true" />}{STATUS_LABELS[report.status]}</span>
      {report.error && <span>{report.error}</span>}
      {report.delivery && report.status === "succeeded" && <span title={report.delivery_detail ?? undefined}>{DELIVERY_LABELS[report.delivery]}{report.delivery_detail && report.delivery !== "pending" ? `: ${report.delivery_detail}` : ""}</span>}
    </>
  );
}

// One report's tables, each downloadable as CSV.
function OpenedReport({ report, onClose }: { report: Report; onClose: () => void }) {
  return (
    <section className="agent-panel report-opened" aria-label={report.title ?? "Report"}>
      <header>
        <strong>{report.title}</strong>
        <span>{report.scope_label} · {report.period_label}</span>
        <button type="button" className="close-session-button" onClick={onClose} aria-label="Close"><X size={16} /></button>
      </header>
      {report.status !== "succeeded" ? <p>{report.error ?? "This report isn't ready yet."}</p> : (
        <>
          <p><a className="primary-button" href={reportPage(report.id)} target="_blank" rel="noreferrer"><ExternalLink size={15} aria-hidden="true" /> Open the report</a></p>
          <ul>
            {(report.tables ?? []).map((table) => (
              <li key={table.id}><a href={tableDownload(report.id, table.id)}><Download size={14} aria-hidden="true" /> {table.section}: {table.title}</a> <span>{table.rows} rows</span></li>
            ))}
          </ul>
        </>
      )}
    </section>
  );
}
