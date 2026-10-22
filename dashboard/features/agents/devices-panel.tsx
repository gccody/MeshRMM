
import { CircleAlert, X } from "lucide-react";
import { useSearchParams } from "react-router";
import { useEffect, useMemo, useRef, useState } from "react";
import { MoveDeviceDialog } from "../companies/company-dialogs";
import { EnrollmentModal } from "../enrollment/enrollment-modal";
import { useInstallerDownload } from "../enrollment/use-installer-download";
import { ConnectionReasonModal } from "../session/connection-reason-modal";
import { ViewerLaunchNotice } from "../session/viewer-launch-notice";
import { StartJobModal } from "../automation/start-job-modal";
import { RunScriptModal } from "../toolbox/run-script-modal";
import { type ActionErrorSource, listActionErrors } from "../workspace/action-errors";
import { useWorkspace } from "../workspace/workspace-context";
import { AgentOverview, type DeviceActions } from "./agent-overview";
import type { Agent } from "./types";
import {
  type AgentStatusFilter,
  type CompanyFilter,
  type DeviceFilters,
  NO_COMPANY,
  clampDeviceQuery,
  filterAgents,
  parseDeviceFilters,
  serializeDeviceFilters,
} from "./device-filters";

// Errors from the actions this page starts. A session-resume error shows in
// the paused card instead.
const DEVICE_ACTIONS: readonly ActionErrorSource[] = ["remote", "close-session", "delete"];

// Safari rate-limits history updates, so typing reaches the URL in batches.
const QUERY_WRITE_DELAY_MS = 250;

export function DevicesPanel() {
  const { account, authorizedFetch, can, inventory, remote, deleteAgent, deletingId, setDevicesSearch, actionErrors, reportActionError } = useWorkspace();
  const installer = useInstallerDownload(authorizedFetch);
  const [isEnrolling, setIsEnrolling] = useState(false);
  // The control that opened the enrollment dialog gets focus back.
  const enrollOpener = useRef<HTMLElement | null>(null);
  const [searchParams, setSearchParams] = useSearchParams();
  // Filters replace the history entry, so Back leaves the page.
  const writeFilters = (next: DeviceFilters) => setSearchParams(new URLSearchParams(serializeDeviceFilters(next)), { replace: true });
  const allowed: DeviceActions = {
    connect: can("sessions.connect"),
    connectBackground: can("sessions.connect_background"),
    closeSession: can("sessions.connect") || can("sessions.close_any"),
    runScripts: can("scripts.run"),
    deliverFiles: can("files.deliver"),
    delete: can("devices.delete"),
    enroll: can("devices.enroll"),
    moveCompany: can("companies.manage"),
  };
  // A connection the device's user must approve, waiting for its reason.
  const [pendingConnect, setPendingConnect] = useState<{ agent: Agent; background: boolean } | null>(null);
  // The device a script is about to run on, and the button that asked.
  const [runScriptOn, setRunScriptOn] = useState<Agent | null>(null);
  const runScriptOpener = useRef<HTMLElement | null>(null);
  // The devices chosen for a job, the job being set up, and its opener.
  const [selected, setSelected] = useState<ReadonlySet<string>>(() => new Set());
  const [startingJob, setStartingJob] = useState<"script" | "file" | null>(null);
  const jobOpener = useRef<HTMLElement | null>(null);
  // The device being moved to another company, and the menu that asked.
  const [moving, setMoving] = useState<Agent | null>(null);
  const moveOpener = useRef<HTMLElement | null>(null);
  const requestConnect = (agent: Agent, background: boolean) => {
    if (account.connection_approval) setPendingConnect({ agent, background });
    else void remote.connect(agent, background);
  };
  const filters = parseDeviceFilters(searchParams);
  const { status, company } = filters;
  // The search box updates at once; the URL follows after a pause in typing.
  const [queryInput, setQueryInput] = useState(filters.query);
  const [querySource, setQuerySource] = useState(filters.query);

  // Back/forward or a Devices link changes the URL under the search box.
  if (filters.query !== querySource) {
    setQuerySource(filters.query);
    if (filters.query !== queryInput) setQueryInput(filters.query);
  }

  useEffect(() => {
    if (queryInput === filters.query) return;
    const timer = window.setTimeout(() => writeFilters({ query: queryInput, status, company }), QUERY_WRITE_DELAY_MS);
    return () => window.clearTimeout(timer);
    // writeFilters changes with the URL, which this effect already follows.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [filters.query, queryInput, status, company]);

  // The Devices link carries the filters, including any not yet in the URL.
  const search = serializeDeviceFilters({ query: queryInput, status, company });
  useEffect(() => {
    setDevicesSearch(search);
  }, [search, setDevicesSearch]);

  const changeStatus = (next: AgentStatusFilter) => writeFilters({ query: queryInput, status: next, company });
  const changeCompany = (next: CompanyFilter) => writeFilters({ query: queryInput, status, company: next });
  const filteredAgents = useMemo(
    () => filterAgents(inventory.agents, { query: queryInput, status, company }),
    [inventory.agents, queryInput, status, company],
  );
  // New devices join the company the page shows.
  const enrollInto = company && company !== NO_COMPANY && inventory.companies.some((candidate) => candidate.id === company) ? company : "";

  return (
    <>
      {remote.sessionNotice && <div className="session-notice" role="status">{remote.sessionNotice}</div>}
      {remote.launch && <ViewerLaunchNotice key={remote.launch.attempt} launch={remote.launch} offline={inventory.connection === "offline"} onRetry={() => remote.retryLaunch(inventory.agents)} onDismiss={remote.dismissLaunch} />}
      {listActionErrors(actionErrors, DEVICE_ACTIONS).map(({ source, message }) => (
        <div key={source} className="error-banner" role="alert">
          <CircleAlert size={16} aria-hidden="true" /><span>{message}</span>
          <button onClick={() => reportActionError(source, null)} aria-label="Dismiss"><X size={16} /></button>
        </div>
      ))}
      <AgentOverview
        agents={inventory.agents}
        filteredAgents={filteredAgents}
        inventory={inventory}
        thumbnails={inventory.thumbnails}
        metrics={inventory.metrics}
        onReconnect={inventory.reconnect}
        onRefresh={() => void inventory.refresh()}
        onAddDevice={(opener) => { enrollOpener.current = opener; installer.reset(enrollInto); setIsEnrolling(true); }}
        companies={inventory.companies}
        query={queryInput}
        status={status}
        company={company}
        connectingId={remote.connectingId}
        connectingBackgroundId={remote.connectingBackgroundId}
        deletingId={deletingId}
        closingId={remote.closingId}
        allowed={allowed}
        onQueryChange={(query) => setQueryInput(clampDeviceQuery(query))}
        onStatusChange={changeStatus}
        onCompanyChange={changeCompany}
        onClearFilters={() => { setQueryInput(""); writeFilters({ query: "", status: "all", company: "" }); }}
        onMove={(agent, opener) => { moveOpener.current = opener; setMoving(agent); }}
        onRemote={(agent) => requestConnect(agent, false)}
        onRemoteBackground={(agent) => requestConnect(agent, true)}
        onCloseSession={(agent) => void remote.closeSession(agent)}
        onDelete={(agent) => void deleteAgent(agent)}
        onRunScript={(agent, opener) => { runScriptOpener.current = opener; setRunScriptOn(agent); }}
        selection={{
          ids: selected,
          onChange: (agent, chosen) => setSelected((current) => {
            const next = new Set(current);
            if (chosen) next.add(agent.id);
            else next.delete(agent.id);
            return next;
          }),
          onSelectAll: (agents) => setSelected((current) => new Set([...current, ...agents.map((agent) => agent.id)])),
          onClear: () => setSelected(new Set()),
          onStartJob: (kind, opener) => { jobOpener.current = opener; setStartingJob(kind); },
        }}
      />
      {startingJob && <StartJobModal
        kind={startingJob}
        deviceIds={inventory.agents.filter((agent) => selected.has(agent.id)).map((agent) => agent.id)}
        onClose={() => setStartingJob(null)}
        returnFocus={jobOpener}
      />}
      {isEnrolling && <EnrollmentModal
        platform={installer.platform}
        error={installer.error}
        isDownloading={installer.isDownloading}
        downloaded={installer.downloaded}
        command={installer.command}
        onClose={() => setIsEnrolling(false)}
        onPlatformChange={installer.setPlatform}
        companies={inventory.companies}
        companyId={installer.companyId}
        onCompanyChange={installer.setCompanyId}
        onSubmit={(event) => void installer.download(event)}
        returnFocus={enrollOpener}
      />}
      {runScriptOn && <RunScriptModal
        agents={inventory.agents}
        initialAgentId={runScriptOn.id}
        onClose={() => setRunScriptOn(null)}
        returnFocus={runScriptOpener}
      />}
      {moving && <MoveDeviceDialog agent={moving} returnFocus={moveOpener} onClose={() => setMoving(null)} />}
      {pendingConnect && <ConnectionReasonModal
        agentName={pendingConnect.agent.name}
        background={pendingConnect.background}
        onCancel={() => setPendingConnect(null)}
        onConnect={(reason) => {
          setPendingConnect(null);
          void remote.connect(pendingConnect.agent, pendingConnect.background, reason);
        }}
      />}
    </>
  );
}
