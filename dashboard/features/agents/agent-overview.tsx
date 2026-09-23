"use client";

import {
  ArrowUpRight,
  ChevronDown,
  Filter,
  LoaderCircle,
  Monitor,
  Square,
  Search,
  Trash2,
  Wifi,
  WifiOff,
} from "lucide-react";
import type { Agent } from "./types";

export type AgentStatusFilter = "all" | "online" | "offline";

type Props = {
  agents: Agent[];
  filteredAgents: Agent[];
  isLive: boolean;
  lastUpdated: Date;
  query: string;
  status: AgentStatusFilter;
  connectingId: string | null;
  connectingBackgroundId: string | null;
  deletingId: string | null;
  closingId: string | null;
  canDelete: boolean;
  onQueryChange: (query: string) => void;
  onStatusChange: (status: AgentStatusFilter) => void;
  onRemote: (agent: Agent) => void;
  onRemoteBackground: (agent: Agent) => void;
  onCloseSession: (agent: Agent) => void;
  onDelete: (agent: Agent) => void;
};

export function AgentOverview({
  agents,
  filteredAgents,
  isLive,
  lastUpdated,
  query,
  status,
  connectingId,
  connectingBackgroundId,
  deletingId,
  closingId,
  canDelete,
  onQueryChange,
  onStatusChange,
  onRemote,
  onRemoteBackground,
  onCloseSession,
  onDelete,
}: Props) {
  const online = agents.filter((agent) => agent.connected).length;
  const offline = agents.length - online;
  const hasFilters = Boolean(query.trim() || status !== "all");

  return (
    <>
      <section className="metrics-grid" aria-label="Device summary">
        {([
          ["all", "All devices", agents.length, Monitor, "purple"],
          ["online", "Online", online, Wifi, "green"],
          ["offline", "Offline", offline, WifiOff, "amber"],
        ] as const).map(([filter, label, count, Icon, color]) => (
          <button key={filter} className={`metric-card ${status === filter ? "metric-selected" : ""}`} onClick={() => onStatusChange(filter)} aria-pressed={status === filter}>
            <div className={`metric-icon ${color}`}><Icon size={21} /></div>
            <div><span>{label}</span><strong>{isLive ? count : "—"}</strong></div>
            <ArrowUpRight size={17} className="metric-arrow" aria-hidden="true" />
          </button>
        ))}
      </section>

      <section className="agent-panel">
        <div className="panel-header"><div><h2>Device inventory</h2><span>{isLive ? `${filteredAgents.length} of ${agents.length} devices` : "Updating…"}</span></div></div>
        <div className="table-toolbar">
          <label className="agent-search"><Search size={18} /><input aria-label="Search devices" value={query} onChange={(event) => onQueryChange(event.target.value)} placeholder="Search by name or device ID" /></label>
          <div className="status-filter"><Filter size={16} /><select value={status} onChange={(event) => onStatusChange(event.target.value as AgentStatusFilter)} aria-label="Filter by status"><option value="all">All statuses</option><option value="online">Online</option><option value="offline">Offline</option></select><ChevronDown size={14} /></div>
        </div>
        <table className="agent-table" aria-label="Managed devices">
          <thead><tr className="table-head"><th scope="col">Device</th><th scope="col">Status</th><th scope="col"><span className="sr-only">Actions</span></th></tr></thead>
          <tbody>
            {filteredAgents.map((agent) => (
              <tr className="agent-row" key={agent.id}>
                <td className="device-cell">
                  <div className={`device-avatar ${agent.connected ? "online" : ""}`}><Monitor size={20} /><span /></div>
                  <div className="device-identity"><strong title={agent.name}>{agent.name}</strong><details className="device-details"><summary>Device ID</summary><code>{agent.id}</code></details></div>
                </td>
                <td className="device-status"><span className={`status-badge ${agent.connected ? "online" : "offline"}`}><i />{agent.connected ? "Online" : "Offline"}</span></td>
                <td className="row-actions">
                  <button className="remote-button" disabled={!agent.connected || connectingId === agent.id || deletingId === agent.id || closingId === agent.id} onClick={() => onRemote(agent)} aria-label={`Connect to ${agent.name}`}>{connectingId === agent.id && connectingBackgroundId !== agent.id ? <LoaderCircle size={16} className="spin" /> : <Monitor size={16} />}{connectingId === agent.id && connectingBackgroundId !== agent.id ? "Connecting…" : "Connect"}</button>
                  <button className="remote-button background-remote-button" disabled={!agent.connected || connectingId === agent.id || deletingId === agent.id || closingId === agent.id} onClick={() => onRemoteBackground(agent)} aria-label={`Connect to ${agent.name} in background mode`} title="Open the private background workspace without changing the user's desktop">{connectingBackgroundId === agent.id ? <LoaderCircle size={16} className="spin" /> : null}{connectingBackgroundId === agent.id ? "Opening…" : "Background"}</button>
                  {canDelete && <button className="close-session-button" disabled={closingId !== null || connectingId === agent.id || deletingId === agent.id} onClick={() => onCloseSession(agent)} aria-label={`Close active session for ${agent.name}`} title="Close active session">{closingId === agent.id ? <LoaderCircle size={16} className="spin" /> : <Square size={16} />}<span className="sr-only">Close session</span></button>}
                  {canDelete && <button className="agent-delete-button" disabled={deletingId === agent.id || closingId === agent.id} onClick={() => onDelete(agent)} aria-label={`Delete ${agent.name}`} title="Delete device">{deletingId === agent.id ? <LoaderCircle size={16} className="spin" /> : <Trash2 size={16} />}</button>}
                </td>
              </tr>
            ))}
          </tbody>
        </table>
        {!filteredAgents.length && <div className="empty-state"><Monitor size={28} /><strong>{!isLive ? "Connecting to your devices…" : hasFilters ? "No matching devices" : "Your devices will appear here"}</strong><span>{!isLive ? "If this takes longer than expected, try Refresh." : hasFilters ? "Try another name or change the status filter." : canDelete ? "Choose Add device to set up your first computer." : "Ask your administrator to add a device."}</span>{hasFilters && <button className="secondary-button" onClick={() => { onQueryChange(""); onStatusChange("all"); }}>Clear filters</button>}</div>}
        <div className="panel-footer"><span>{isLive ? `Updated ${lastUpdated.toLocaleTimeString([], { hour: "numeric", minute: "2-digit" })}` : "Waiting for updates"}</span><span className={isLive ? "" : "disconnected"}><i />{isLive ? "Updates automatically" : "Reconnecting…"}</span></div>
      </section>
    </>
  );
}
