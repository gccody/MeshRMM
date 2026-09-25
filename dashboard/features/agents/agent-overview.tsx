"use client";

import { useEffect, useId, useRef, useState } from "react";
import {
  ArrowUpRight,
  ChevronDown,
  Filter,
  Layers,
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

function ConnectMenu({ agent, disabled, connecting, background, onRemote, onRemoteBackground }: {
  agent: Agent;
  disabled: boolean;
  connecting: boolean;
  background: boolean;
  onRemote: (agent: Agent) => void;
  onRemoteBackground: (agent: Agent) => void;
}) {
  const [open, setOpen] = useState(false);
  const containerRef = useRef<HTMLDivElement>(null);
  const triggerRef = useRef<HTMLButtonElement>(null);
  const menuId = useId();

  useEffect(() => {
    if (!open) return;
    const closeOnOutsideClick = (event: PointerEvent) => {
      if (!containerRef.current?.contains(event.target as Node)) setOpen(false);
    };
    const closeOnEscape = (event: KeyboardEvent) => {
      if (event.key !== "Escape") return;
      setOpen(false);
      triggerRef.current?.focus();
    };
    document.addEventListener("pointerdown", closeOnOutsideClick);
    document.addEventListener("keydown", closeOnEscape);
    return () => {
      document.removeEventListener("pointerdown", closeOnOutsideClick);
      document.removeEventListener("keydown", closeOnEscape);
    };
  }, [open]);

  const choose = (action: (agent: Agent) => void) => {
    setOpen(false);
    action(agent);
  };

  return (
    <div className={`connect-menu${open ? " open" : ""}`} ref={containerRef}>
      <button
        ref={triggerRef}
        type="button"
        className="remote-button connect-trigger"
        disabled={disabled}
        aria-label={`Connect to ${agent.name}`}
        aria-expanded={open}
        aria-controls={open ? menuId : undefined}
        onClick={() => setOpen((current) => !current)}
      >
        {connecting ? <LoaderCircle size={16} className="spin" /> : <Monitor size={16} />}
        {connecting ? (background ? "Opening…" : "Connecting…") : "Connect"}
        {!connecting && <ChevronDown size={14} aria-hidden="true" />}
      </button>
      {open && <div id={menuId} className="connect-options" role="group" aria-label={`Connection options for ${agent.name}`}>
        <button type="button" onClick={() => choose(onRemote)}><Monitor size={16} aria-hidden="true" />Connect</button>
        <button type="button" onClick={() => choose(onRemoteBackground)} title="Open the private background workspace without changing the user's desktop"><Layers size={16} aria-hidden="true" />Connect to background</button>
      </div>}
    </div>
  );
}

function StatusBadge({ agent }: { agent: Agent }) {
  if (agent.connected) return <span className="status-badge online"><i />Online</span>;
  if (agent.updating_to) {
    const detail = `Installing Agent ${agent.updating_to}. The device reconnects when the update finishes.`;
    return <span className="status-badge updating" title={detail}><i />Updating<span className="sr-only">. {detail}</span></span>;
  }
  return <span className="status-badge offline"><i />Offline</span>;
}

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
                <td className="device-status"><StatusBadge agent={agent} /></td>
                <td className="row-actions">
                  <ConnectMenu agent={agent} disabled={!agent.connected || connectingId === agent.id || deletingId === agent.id || closingId === agent.id} connecting={connectingId === agent.id} background={connectingBackgroundId === agent.id} onRemote={onRemote} onRemoteBackground={onRemoteBackground} />
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
