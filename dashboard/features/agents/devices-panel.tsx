"use client";

import { useSearchParams } from "next/navigation";
import { useEffect, useMemo, useState } from "react";
import { useWorkspace } from "../workspace/workspace-context";
import { AgentOverview } from "./agent-overview";
import {
  type AgentStatusFilter,
  type DeviceFilters,
  clampDeviceQuery,
  filterAgents,
  parseDeviceFilters,
  serializeDeviceFilters,
} from "./device-filters";

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
  const { inventory, remote, deleteAgent, deletingId, isAdmin, setDevicesSearch } = useWorkspace();
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
      <AgentOverview
        agents={inventory.agents}
        filteredAgents={filteredAgents}
        isLive={inventory.isLive}
        lastUpdated={inventory.lastUpdated}
        query={queryInput}
        status={status}
        connectingId={remote.connectingId}
        connectingBackgroundId={remote.connectingBackgroundId}
        deletingId={deletingId}
        closingId={remote.closingId}
        canDelete={isAdmin}
        onQueryChange={(query) => setQueryInput(clampDeviceQuery(query))}
        onStatusChange={changeStatus}
        onRemote={(agent) => void remote.connect(agent)}
        onRemoteBackground={(agent) => void remote.connect(agent, true)}
        onCloseSession={(agent) => void remote.closeSession(agent)}
        onDelete={(agent) => void deleteAgent(agent)}
      />
    </>
  );
}
