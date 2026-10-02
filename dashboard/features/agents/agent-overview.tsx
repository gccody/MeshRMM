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
  SquareTerminal,
  Trash2,
  Wifi,
  WifiOff,
} from "lucide-react";
import type { AgentStatusFilter } from "./device-filters";
import { DeviceThumbnail } from "./device-thumbnail";
import type { InventoryConnection, InventoryStatus } from "./inventory-stream";
import type { ThumbnailStore } from "./thumbnails";
import type { Agent } from "./types";

type InventoryState = {
  status: InventoryStatus;
  connection: InventoryConnection;
  lastUpdated: Date | null;
  error: string | null;
};

type Props = {
  agents: Agent[];
  filteredAgents: Agent[];
  inventory: InventoryState;
  thumbnails: ThumbnailStore;
  onReconnect: () => void;
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
  onRunScript: (agent: Agent, opener: HTMLElement) => void;
};

const formatTime = (date: Date | null) =>
  date?.toLocaleTimeString([], { hour: "numeric", minute: "2-digit" }) ?? "";

function ConnectMenu({ agent, disabled, title, connecting, background, onRemote, onRemoteBackground }: {
  agent: Agent;
  disabled: boolean;
  title?: string;
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
    // The chosen option disappears with the menu; a dialog the action opens
    // returns focus here.
    triggerRef.current?.focus();
    action(agent);
  };

  return (
    <div className={`connect-menu${open ? " open" : ""}`} ref={containerRef}>
      <button
        ref={triggerRef}
        type="button"
        className="remote-button connect-trigger"
        disabled={disabled}
        title={title}
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

function StatusBadge({ agent, stale }: { agent: Agent; stale: boolean }) {
  if (agent.connected) return <span className="status-badge online"><i />{stale ? <>Online<span className="status-badge-note"><span className="status-badge-separator"> · </span>last known</span></> : "Online"}</span>;
  if (agent.updating_to) {
    const detail = `Installing Agent ${agent.updating_to}. The device reconnects when the update finishes.`;
    return <span className="status-badge updating" title={detail}><i />Updating<span className="sr-only">. {detail}</span></span>;
  }
  return <span className="status-badge offline"><i />Offline</span>;
}

// Explains why the list may be out of date and offers to reconnect.
function InventoryStrip({ inventory, onReconnect }: { inventory: InventoryState; onReconnect: () => void }) {
  const { status, connection, lastUpdated, error } = inventory;
  const offline = connection === "offline";
  const stale = status === "stale";
  // While offline, the failed requests only repeat that.
  const detail = offline ? null : error;
  if (!stale && !detail) return null;
  const time = formatTime(lastUpdated);
  const headline = !stale ? null
    : offline ? `You’re offline. Showing devices from ${time}.`
    : connection === "unavailable" ? `Live updates are unavailable. Showing devices as of ${time}.`
    : `Live updates are reconnecting. Showing devices as of ${time}.`;
  return (
    <div className="inventory-strip" role="status">
      <WifiOff size={17} aria-hidden="true" />
      <span>{headline}{headline && detail ? " " : null}{detail && <span className="inventory-strip-detail">{detail}</span>}</span>
      {!offline && connection !== "live" && <button type="button" className="secondary-button" onClick={onReconnect}>Reconnect now</button>}
    </div>
  );
}

export function AgentOverview({
  agents,
  filteredAgents,
  inventory,
  thumbnails,
  onReconnect,
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
  onRunScript,
}: Props) {
  const online = agents.filter((agent) => agent.connected).length;
  const offline = agents.length - online;
  const hasFilters = Boolean(query.trim() || status !== "all");
  const hasData = inventory.status !== "loading";
  const stale = inventory.status === "stale";
  const isOffline = inventory.connection === "offline";
  const time = formatTime(inventory.lastUpdated);
  const [footerTime, footerState] = !hasData ? ["Waiting for updates", isOffline ? "Offline" : inventory.connection === "unavailable" ? "Live updates unavailable" : "Connecting…"]
    : !stale ? [`Updated ${time}`, "Updates automatically"]
    : isOffline ? [`You’re offline · showing devices from ${time}`, "Offline"]
    : inventory.connection === "unavailable" ? [`Last updated ${time}`, "Live updates unavailable"]
    : [`Last updated ${time}`, "Reconnecting…"];
  // The handoff API checks the device itself, so a stale list only warns.
  const connectTitle = isOffline ? "You’re offline" : stale ? "Status may be out of date" : undefined;

  return (
    <>
      <InventoryStrip inventory={inventory} onReconnect={onReconnect} />
      <section className="metrics-grid" aria-label="Device summary">
        {([
          ["all", "All devices", agents.length, Monitor, "purple"],
          ["online", "Online", online, Wifi, "green"],
          ["offline", "Offline", offline, WifiOff, "amber"],
        ] as const).map(([filter, label, count, Icon, color]) => (
          <button key={filter} className={`metric-card ${status === filter ? "metric-selected" : ""}`} onClick={() => onStatusChange(filter)} aria-pressed={status === filter}>
            <div className={`metric-icon ${color}`}><Icon size={21} /></div>
            <div><span>{label}</span><strong>{hasData ? count : "—"}</strong></div>
            <ArrowUpRight size={17} className="metric-arrow" aria-hidden="true" />
          </button>
        ))}
      </section>

      <section className="agent-panel">
        <div className="panel-header"><div><h2>Device inventory</h2><span>{hasData ? `${filteredAgents.length} of ${agents.length} devices` : "Loading…"}</span></div></div>
        <div className="table-toolbar">
          <label className="agent-search"><Search size={18} /><input aria-label="Search devices" value={query} onChange={(event) => onQueryChange(event.target.value)} placeholder="Search by name or device ID" /></label>
          <div className="status-filter"><Filter size={16} /><select value={status} onChange={(event) => onStatusChange(event.target.value as AgentStatusFilter)} aria-label="Filter by status"><option value="all">All statuses</option><option value="online">Online</option><option value="offline">Offline</option></select><ChevronDown size={14} /></div>
        </div>
        <table className="agent-table" aria-label="Managed devices">
          <thead><tr className="table-head"><th scope="col">Device</th><th scope="col">Status</th><th scope="col"><span className="sr-only">Actions</span></th></tr></thead>
          <tbody>
            {filteredAgents.map((agent) => (
              <tr className={`agent-row${stale ? " stale" : ""}`} key={agent.id}>
                <td className="device-cell">
                  <DeviceThumbnail agent={agent} store={thumbnails} />
                  <div className="device-identity"><strong title={agent.name}>{agent.name}</strong><details className="device-details"><summary>Device ID</summary><code>{agent.id}</code></details></div>
                </td>
                <td className="device-status"><StatusBadge agent={agent} stale={stale} /></td>
                <td className="row-actions">
                  <ConnectMenu agent={agent} disabled={!agent.connected || isOffline || connectingId === agent.id || deletingId === agent.id || closingId === agent.id} title={agent.connected ? connectTitle : undefined} connecting={connectingId === agent.id} background={connectingBackgroundId === agent.id} onRemote={onRemote} onRemoteBackground={onRemoteBackground} />
                  <button className="close-session-button" disabled={!agent.connected || isOffline || deletingId === agent.id} onClick={(event) => onRunScript(agent, event.currentTarget)} aria-label={`Run a script on ${agent.name}`} aria-haspopup="dialog" title="Run a script"><SquareTerminal size={16} /><span className="sr-only">Run a script</span></button>
                  {canDelete && <button className="close-session-button" disabled={closingId !== null || connectingId === agent.id || deletingId === agent.id} onClick={() => onCloseSession(agent)} aria-label={`Close active session for ${agent.name}`} title="Close active session">{closingId === agent.id ? <LoaderCircle size={16} className="spin" /> : <Square size={16} />}<span className="sr-only">Close session</span></button>}
                  {canDelete && <button className="agent-delete-button" disabled={deletingId === agent.id || closingId === agent.id} onClick={() => onDelete(agent)} aria-label={`Delete ${agent.name}`} title="Delete device">{deletingId === agent.id ? <LoaderCircle size={16} className="spin" /> : <Trash2 size={16} />}</button>}
                </td>
              </tr>
            ))}
          </tbody>
        </table>
        {!filteredAgents.length && <div className="empty-state"><Monitor size={28} /><strong>{!hasData ? "Connecting to your devices…" : hasFilters ? "No matching devices" : "Your devices will appear here"}</strong><span>{!hasData ? "If this takes longer than expected, try Refresh." : hasFilters ? "Try another name or change the status filter." : canDelete ? "Choose Add device to set up your first computer." : "Ask your administrator to add a device."}</span>{hasFilters && <button className="secondary-button" onClick={() => { onQueryChange(""); onStatusChange("all"); }}>Clear filters</button>}</div>}
        <div className="panel-footer"><span>{footerTime}</span><span className={hasData && !stale ? "" : "disconnected"}><i />{footerState}</span></div>
      </section>
    </>
  );
}
