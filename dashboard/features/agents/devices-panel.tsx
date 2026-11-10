import { CircleAlert, X } from "lucide-react";
import { useSearchParams } from "react-router";
import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { MoveDeviceDialog } from "../companies/company-dialogs";
import { EnrollmentModal } from "../enrollment/enrollment-modal";
import { useInstallerDownload } from "../enrollment/use-installer-download";
import { ConnectionReasonModal } from "../session/connection-reason-modal";
import { ViewerLaunchNotice } from "../session/viewer-launch-notice";
import { StartJobModal } from "../automation/start-job-modal";
import { TagDevicesDialog } from "../tags/tag-dialogs";
import type { TagMatch } from "../tags/model";
import { RunScriptModal } from "../toolbox/run-script-modal";
import { WakeDialog } from "../wake/wake-dialog";
import { type ActionErrorSource, listActionErrors } from "../workspace/action-errors";
import { useWorkspace } from "../workspace/workspace-context";
import { AgentOverview, type DeviceActions } from "./agent-overview";
import { chosenAgents } from "./model";
import type { Agent } from "./types";
import { useCompanies, useDevices, useTags } from "./use-devices";
import {
  type AgentStatusFilter,
  type CompanyFilter,
  DEFAULT_DEVICE_FILTERS,
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
  const { devices } = inventory;
  // The list, but not the readings: those redraw only their own tiles.
  const agents = useDevices(devices);
  const companies = useCompanies(devices);
  const deviceTags = useTags(devices);
  const installer = useInstallerDownload(authorizedFetch);
  const [isEnrolling, setIsEnrolling] = useState(false);
  // The control that opened the enrollment dialog gets focus back.
  const enrollOpener = useRef<HTMLElement | null>(null);
  const [searchParams, setSearchParams] = useSearchParams();
  // Filters replace the history entry, so Back leaves the page.
  const writeFilters = (next: DeviceFilters) => setSearchParams(new URLSearchParams(serializeDeviceFilters(next)), { replace: true });
  // The same object until the account changes, so the tiles don't redraw.
  const allowed = useMemo<DeviceActions>(() => ({
    connect: can("sessions.connect"),
    connectBackground: can("sessions.connect_background"),
    closeSession: can("sessions.connect") || can("sessions.close_any"),
    runScripts: can("scripts.run"),
    terminal: can("terminal.connect") || can("terminal.connect_system"),
    files: can("files.browse"),
    wake: can("devices.wake"),
    deliverFiles: can("files.deliver"),
    delete: can("devices.delete"),
    enroll: can("devices.enroll"),
    moveCompany: can("companies.manage"),
    editTags: can("devices.edit"),
  }), [can]);
  // A connection the device's user must approve, waiting for its reason.
  const [pendingConnect, setPendingConnect] = useState<{ agent: Agent; background: boolean } | null>(null);
  // The device a script is about to run on, and the button that asked.
  const [runScriptOn, setRunScriptOn] = useState<Agent | null>(null);
  const runScriptOpener = useRef<HTMLElement | null>(null);
  // The devices chosen for a job, the job being set up, and its opener.
  const [selected, setSelected] = useState<ReadonlySet<string>>(() => new Set());
  const [startingJob, setStartingJob] = useState<"script" | "file" | null>(null);
  const jobOpener = useRef<HTMLElement | null>(null);
  // The offline devices being woken, and the control that asked.
  const [waking, setWaking] = useState<Agent[] | null>(null);
  const wakeOpener = useRef<HTMLElement | null>(null);
  // The device being moved to another company, and the menu that asked.
  const [moving, setMoving] = useState<Agent | null>(null);
  const moveOpener = useRef<HTMLElement | null>(null);
  // The devices whose tags are being edited, and the control that asked.
  const [tagging, setTagging] = useState<Agent[] | null>(null);
  const tagOpener = useRef<HTMLElement | null>(null);

  // The tiles' actions keep their identity, so a tile redraws only when its
  // own device, reading or state changes.
  const { connect, closeSession } = remote;
  const approval = account.connection_approval;
  const onRemote = useCallback((agent: Agent) => {
    if (approval) setPendingConnect({ agent, background: false });
    else void connect(agent, false);
  }, [approval, connect]);
  const onRemoteBackground = useCallback((agent: Agent) => {
    if (approval) setPendingConnect({ agent, background: true });
    else void connect(agent, true);
  }, [approval, connect]);
  const onCloseSession = useCallback((agent: Agent) => void closeSession(agent), [closeSession]);
  const onDelete = useCallback((agent: Agent) => void deleteAgent(agent), [deleteAgent]);
  const onRunScript = useCallback((agent: Agent, opener: HTMLElement) => { runScriptOpener.current = opener; setRunScriptOn(agent); }, []);
  const onWake = useCallback((chosen: Agent[], opener: HTMLElement) => { wakeOpener.current = opener; setWaking(chosen); }, []);
  const onMove = useCallback((agent: Agent, opener: HTMLElement) => { moveOpener.current = opener; setMoving(agent); }, []);
  const onEditTags = useCallback((chosen: Agent[], opener: HTMLElement) => { tagOpener.current = opener; setTagging(chosen); }, []);
  const onSelect = useCallback((agent: Agent, chosen: boolean) => setSelected((current) => {
    const next = new Set(current);
    if (chosen) next.add(agent.id);
    else next.delete(agent.id);
    return next;
  }), []);

  const filters = parseDeviceFilters(searchParams);
  const { status, company, tags, tagMatch } = filters;
  // A stable key for the chosen tags, which the URL parses afresh each render.
  const tagKey = tags.join(",");
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
    const timer = window.setTimeout(() => writeFilters({ ...filters, query: queryInput }), QUERY_WRITE_DELAY_MS);
    return () => window.clearTimeout(timer);
    // writeFilters changes with the URL, which this effect already follows.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [filters.query, queryInput, status, company, tagKey, tagMatch]);

  // The Devices link carries the filters, including any not yet in the URL.
  const search = serializeDeviceFilters({ ...filters, query: queryInput });
  useEffect(() => {
    setDevicesSearch(search);
  }, [search, setDevicesSearch]);

  const changeStatus = (next: AgentStatusFilter) => writeFilters({ ...filters, query: queryInput, status: next });
  const changeCompany = (next: CompanyFilter) => writeFilters({ ...filters, query: queryInput, company: next });
  const changeTags = (next: string[], match: TagMatch) => writeFilters({ ...filters, query: queryInput, tags: next, tagMatch: match });
  const filteredAgents = useMemo(
    () => filterAgents(agents, { query: queryInput, status, company, tags: tagKey ? tagKey.split(",") : [], tagMatch }, deviceTags),
    [agents, deviceTags, queryInput, status, company, tagKey, tagMatch],
  );
  // New devices join the company the page shows.
  const enrollInto = company && company !== NO_COMPANY && companies.some((candidate) => candidate.id === company) ? company : "";

  return (
    <>
      {remote.sessionNotice && <div className="session-notice" role="status">{remote.sessionNotice}</div>}
      {remote.launch && <ViewerLaunchNotice key={remote.launch.attempt} launch={remote.launch} offline={inventory.connection === "offline"} onRetry={() => remote.retryLaunch((id) => devices.get(id))} onDismiss={remote.dismissLaunch} />}
      {listActionErrors(actionErrors, DEVICE_ACTIONS).map(({ source, message }) => (
        <div key={source} className="error-banner" role="alert">
          <CircleAlert size={16} aria-hidden="true" /><span>{message}</span>
          <button onClick={() => reportActionError(source, null)} aria-label="Dismiss"><X size={16} /></button>
        </div>
      ))}
      <AgentOverview
        filteredAgents={filteredAgents}
        inventory={inventory}
        onReconnect={inventory.reconnect}
        onRefresh={() => void inventory.refresh()}
        onAddDevice={(opener) => { enrollOpener.current = opener; installer.reset(enrollInto); setIsEnrolling(true); }}
        companies={companies}
        tags={deviceTags}
        query={queryInput}
        status={status}
        company={company}
        tagFilter={tags}
        tagMatch={tagMatch}
        connectingId={remote.connectingId}
        connectingBackgroundId={remote.connectingBackgroundId}
        deletingId={deletingId}
        closingId={remote.closingId}
        allowed={allowed}
        onQueryChange={(query) => setQueryInput(clampDeviceQuery(query))}
        onStatusChange={changeStatus}
        onCompanyChange={changeCompany}
        onTagsChange={changeTags}
        onClearFilters={() => { setQueryInput(""); writeFilters(DEFAULT_DEVICE_FILTERS); }}
        onEditTags={onEditTags}
        onMove={onMove}
        onRemote={onRemote}
        onRemoteBackground={onRemoteBackground}
        onCloseSession={onCloseSession}
        onDelete={onDelete}
        onRunScript={onRunScript}
        onWake={onWake}
        selection={{
          ids: selected,
          onChange: onSelect,
          onSelectAll: (shown) => setSelected((current) => new Set([...current, ...shown.map((agent) => agent.id)])),
          onClear: () => setSelected(new Set()),
          onStartJob: (kind, opener) => { jobOpener.current = opener; setStartingJob(kind); },
        }}
      />
      {startingJob && <StartJobModal
        kind={startingJob}
        deviceIds={chosenAgents(selected, devices).map((agent) => agent.id)}
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
        companies={companies}
        companyId={installer.companyId}
        onCompanyChange={installer.setCompanyId}
        onSubmit={(event) => void installer.download(event)}
        returnFocus={enrollOpener}
      />}
      {runScriptOn && <RunScriptModal
        initialAgentId={runScriptOn.id}
        onClose={() => setRunScriptOn(null)}
        returnFocus={runScriptOpener}
      />}
      {waking && <WakeDialog agents={waking} returnFocus={wakeOpener} onClose={() => setWaking(null)} />}
      {tagging && <TagDevicesDialog agents={tagging} returnFocus={tagOpener} onClose={() => setTagging(null)} />}
      {moving && <MoveDeviceDialog agent={moving} returnFocus={moveOpener} onClose={() => setMoving(null)} />}
      {pendingConnect && <ConnectionReasonModal
        agentName={pendingConnect.agent.name}
        background={pendingConnect.background}
        onCancel={() => setPendingConnect(null)}
        onConnect={(reason) => {
          setPendingConnect(null);
          void connect(pendingConnect.agent, pendingConnect.background, reason);
        }}
      />}
    </>
  );
}
