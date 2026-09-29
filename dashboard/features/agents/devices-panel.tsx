"use client";

import { CircleAlert, X } from "lucide-react";
import { useSearchParams } from "next/navigation";
import { useEffect, useMemo, useState } from "react";
import { ConnectionReasonModal } from "../session/connection-reason-modal";
import { ViewerLaunchNotice } from "../session/viewer-launch-notice";
import { type ActionErrorSource, listActionErrors } from "../workspace/action-errors";
import { useWorkspace } from "../workspace/workspace-context";
import { AgentOverview } from "./agent-overview";
import type { Agent } from "./types";
import {
  type AgentStatusFilter,
  type DeviceFilters,
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

// Updates the address bar without a server round trip. vinext's patched
// replaceState updates useSearchParams(); router.replace would refetch the page.
// A null state lets the router keep its own history metadata.
function writeFilters(filters: DeviceFilters) {
  const { pathname, hash } = window.location;
  window.history.replaceState(null, "", `${pathname}${serializeDeviceFilters(filters)}${hash}`);
}

export function DevicesPanel() {
  const { inventory, remote, company, deleteAgent, deletingId, isAdmin, setDevicesSearch, actionErrors, reportActionError } = useWorkspace();
  // A connection the device's user must approve, waiting for its reason.
  const [pendingConnect, setPendingConnect] = useState<{ agent: Agent; background: boolean } | null>(null);
  const requestConnect = (agent: Agent, background: boolean) => {
    if (company?.connection_approval) setPendingConnect({ agent, background });
    else void remote.connect(agent, background);
  };
  const filters = parseDeviceFilters(useSearchParams());
  const status = filters.status;
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
    const timer = window.setTimeout(() => writeFilters({ query: queryInput, status }), QUERY_WRITE_DELAY_MS);
    return () => window.clearTimeout(timer);
  }, [filters.query, queryInput, status]);

  // The Devices link carries the filters, including any not yet in the URL.
  const search = serializeDeviceFilters({ query: queryInput, status });
  useEffect(() => {
    setDevicesSearch(search);
  }, [search, setDevicesSearch]);

  const changeStatus = (next: AgentStatusFilter) => writeFilters({ query: queryInput, status: next });
  const filteredAgents = useMemo(
    () => filterAgents(inventory.agents, { query: queryInput, status }),
    [inventory.agents, queryInput, status],
  );

  return (
    <>
      {remote.sessionNotice && <div className="session-notice" role="status">{remote.sessionNotice}</div>}
      {remote.launch && <ViewerLaunchNotice key={remote.launch.attempt} launch={remote.launch} offline={inventory.connection === "offline"} onRetry={() => remote.retryLaunch(inventory.agents)} onDismiss={remote.dismissLaunch} />}
      {listActionErrors(actionErrors, DEVICE_ACTIONS).map(({ source, message }) => (
        <div key={source} className="error-banner" role="alert">
          <CircleAlert size={17} aria-hidden="true" /><span>{message}</span>
          <button onClick={() => reportActionError(source, null)} aria-label="Dismiss"><X size={16} /></button>
        </div>
      ))}
      <AgentOverview
        agents={inventory.agents}
        filteredAgents={filteredAgents}
        inventory={inventory}
        onReconnect={inventory.reconnect}
        query={queryInput}
        status={status}
        connectingId={remote.connectingId}
        connectingBackgroundId={remote.connectingBackgroundId}
        deletingId={deletingId}
        closingId={remote.closingId}
        canDelete={isAdmin}
        onQueryChange={(query) => setQueryInput(clampDeviceQuery(query))}
        onStatusChange={changeStatus}
        onRemote={(agent) => requestConnect(agent, false)}
        onRemoteBackground={(agent) => requestConnect(agent, true)}
        onCloseSession={(agent) => void remote.closeSession(agent)}
        onDelete={(agent) => void deleteAgent(agent)}
      />
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
