"use client";

import {
  Check,
  ChevronDown,
  Clock3,
  Filter,
  LoaderCircle,
  Monitor,
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
  deletingId: string | null;
  canDelete: boolean;
  onQueryChange: (query: string) => void;
  onStatusChange: (status: AgentStatusFilter) => void;
  onRemote: (agent: Agent) => void;
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
  deletingId,
  canDelete,
  onQueryChange,
  onStatusChange,
  onRemote,
  onDelete,
}: Props) {
  const online = agents.filter((agent) => agent.connected).length;
  const offline = agents.length - online;
  const coverage = agents.length ? Math.round((online / agents.length) * 100) : 0;

  return (
    <>
      <section className="metrics-grid" aria-label="Agent summary">
        <article className="metric-card">
          <div className="metric-icon purple"><Monitor size={19} /></div>
          <div><span>Total Agents</span><strong>{isLive ? agents.length : "--"}</strong><small>{isLive ? "Company-owned endpoints" : "Awaiting live service"}</small></div>
        </article>
        <article className="metric-card">
          <div className="metric-icon green"><Wifi size={19} /></div>
          <div><span>Online now</span><strong>{isLive ? online : "--"}</strong><small>{isLive ? <><b>{coverage}%</b> connection rate</> : "Awaiting live service"}</small></div>
          {isLive ? <span className="metric-badge good">Live</span> : null}
        </article>
        <article className="metric-card">
          <div className="metric-icon amber"><WifiOff size={19} /></div>
          <div><span>Offline</span><strong>{isLive ? offline : "--"}</strong><small>{isLive ? "Current signaling state" : "Awaiting live service"}</small></div>
          {isLive ? <span className={`metric-badge ${offline ? "warn" : "good"}`}>{offline ? "Review" : "Clear"}</span> : null}
        </article>
        <article className="metric-card">
          <div className="metric-icon blue"><Clock3 size={19} /></div>
          <div><span>Inventory status</span><strong className="status-word">{isLive ? "Live" : "Unavailable"}</strong><small>{isLive ? `Updated ${lastUpdated.toLocaleTimeString([], { hour: "numeric", minute: "2-digit" })}` : "No live response"}</small></div>
          {isLive ? <Check size={20} className="metric-check" /> : null}
        </article>
      </section>

      <section className="agent-panel">
        <div className="panel-header"><div><h2>Company Agents</h2><span>{filteredAgents.length} of {agents.length} devices</span></div></div>
        <div className="table-toolbar">
          <label className="agent-search"><Search size={17} /><input value={query} onChange={(event) => onQueryChange(event.target.value)} placeholder="Search by name or device ID..." /></label>
          <div className="status-filter"><Filter size={16} /><select value={status} onChange={(event) => onStatusChange(event.target.value as AgentStatusFilter)} aria-label="Filter by status"><option value="all">All statuses</option><option value="online">Online</option><option value="offline">Offline</option></select><ChevronDown size={14} /></div>
        </div>
        <div className="agent-table" role="table" aria-label="Managed Agents">
          <div className="table-head" role="row"><span>Device</span><span>Connection</span><span>Device ID</span><span aria-hidden="true" /></div>
          {filteredAgents.map((agent) => (
            <div className="agent-row" role="row" key={agent.id}>
              <div className="device-cell"><div className={`device-avatar ${agent.connected ? "online" : ""}`}>{(agent.name.match(/[a-z0-9]/i)?.[0] ?? "A").toUpperCase()}<span /></div><div><strong>{agent.name}</strong><code>Cloudflare live inventory</code></div></div>
              <div><span className={`status-badge ${agent.connected ? "online" : "offline"}`}><i />{agent.connected ? "Online" : "Offline"}</span></div>
              <div className="device-id-cell"><code>{agent.id}</code></div>
              <div className="row-actions">
                <button className={`remote-button ${!agent.connected ? "disabled" : ""}`} disabled={!agent.connected || connectingId === agent.id || deletingId === agent.id} onClick={() => onRemote(agent)}>{connectingId === agent.id ? <LoaderCircle size={15} className="spin" /> : <Monitor size={15} />}{connectingId === agent.id ? "Authorizing..." : "Remote"}</button>
                {canDelete && <button className="agent-delete-button" disabled={deletingId === agent.id} onClick={() => onDelete(agent)} aria-label={`Delete ${agent.name}`} title="Delete Agent">{deletingId === agent.id ? <LoaderCircle size={15} className="spin" /> : <Trash2 size={15} />}</button>}
              </div>
            </div>
          ))}
          {!filteredAgents.length && <div className="empty-state"><Search size={24} /><strong>{isLive ? "No Agents found" : "No live Agent data"}</strong><span>{isLive ? "Create an Agent or adjust the current filters." : "The company inventory has not returned a live response."}</span></div>}
        </div>
        <div className="panel-footer"><span>Updates arrive in real time</span><span className={isLive ? "" : "disconnected"}><i /> {isLive ? "Tenant-scoped event stream ready" : "Reconnecting event stream"}</span></div>
      </section>
    </>
  );
}
