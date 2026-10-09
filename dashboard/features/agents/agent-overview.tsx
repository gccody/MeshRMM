import { useRef, useState } from "react";
import { Link, useNavigate } from "react-router";
import {
  Activity,
  Copy,
  Ellipsis,
  Expand,
  Layers,
  LoaderCircle,
  Monitor,
  MonitorOff,
  Plus,
  RefreshCw,
  Search,
  Square,
  SquareTerminal,
  Trash2,
  WifiOff,
} from "lucide-react";
import { usePopover } from "../../lib/use-popover";
import type { MetricsReading } from "../metrics/model";
import { DeviceUsage } from "../metrics/usage-meter";
import { HeaderActions } from "../workspace/header-actions";
import type { AgentStatusFilter } from "./device-filters";
import { ScreenPreview, useDeviceScreen } from "./device-thumbnail";
import type { InventoryConnection, InventoryStatus } from "./inventory-stream";
import type { ThumbnailStore } from "./thumbnails";
import type { Agent } from "./types";

type InventoryState = {
  status: InventoryStatus;
  connection: InventoryConnection;
  lastUpdated: Date | null;
  error: string | null;
  isRefreshing: boolean;
};

type Props = {
  agents: Agent[];
  filteredAgents: Agent[];
  inventory: InventoryState;
  thumbnails: ThumbnailStore;
  // Each online device's latest resource usage.
  metrics: ReadonlyMap<string, MetricsReading>;
  onReconnect: () => void;
  onRefresh: () => void;
  onAddDevice: (opener: HTMLElement) => void;
  query: string;
  status: AgentStatusFilter;
  connectingId: string | null;
  connectingBackgroundId: string | null;
  deletingId: string | null;
  closingId: string | null;
  allowed: DeviceActions;
  onQueryChange: (query: string) => void;
  onStatusChange: (status: AgentStatusFilter) => void;
  onRemote: (agent: Agent) => void;
  onRemoteBackground: (agent: Agent) => void;
  onCloseSession: (agent: Agent) => void;
  onDelete: (agent: Agent) => void;
  onRunScript: (agent: Agent, opener: HTMLElement) => void;
};

// What the signed-in user may do with a device.
export type DeviceActions = {
  connect: boolean;
  connectBackground: boolean;
  closeSession: boolean;
  runScripts: boolean;
  delete: boolean;
  enroll: boolean;
};

type TileState = {
  stale: boolean;
  browserOffline: boolean;
  connecting: boolean;
  background: boolean;
  deleting: boolean;
  closing: boolean;
  anyClosing: boolean;
};

// The device's own page, with its resource usage.
const devicePath = (agent: Agent) => `/device?id=${encodeURIComponent(agent.id)}`;

const formatTime = (date: Date | null) =>
  date?.toLocaleTimeString([], { hour: "numeric", minute: "2-digit" }) ?? "";

// Explains why the wall may be out of date and offers to reconnect.
function InventoryStrip({ inventory, onReconnect }: { inventory: InventoryState; onReconnect: () => void }) {
  const { status, connection, lastUpdated, error } = inventory;
  const offline = connection === "offline";
  const stale = status === "stale";
  // While offline, the failed requests only repeat that.
  const detail = offline ? null : error;
  if (!stale && !detail) return null;
  const time = formatTime(lastUpdated);
  const headline = !stale ? null
    : offline ? `You’re offline. Showing devices as of ${time}.`
    : `Reconnecting. Showing devices as of ${time}.`;
  return (
    <div className="inventory-strip" role="status">
      <WifiOff size={16} aria-hidden="true" />
      <span>{headline}{headline && detail ? " " : null}{detail}</span>
      {!offline && connection !== "live" && <button type="button" className="link-button" onClick={onReconnect}>Reconnect now</button>}
    </div>
  );
}

const FILTERS: { id: AgentStatusFilter; label: string }[] = [
  { id: "all", label: "All" },
  { id: "online", label: "Online" },
  { id: "offline", label: "Offline" },
];

export function AgentOverview({
  agents,
  filteredAgents,
  inventory,
  thumbnails,
  metrics,
  onReconnect,
  onRefresh,
  onAddDevice,
  query,
  status,
  connectingId,
  connectingBackgroundId,
  deletingId,
  closingId,
  allowed,
  onQueryChange,
  onStatusChange,
  onRemote,
  onRemoteBackground,
  onCloseSession,
  onDelete,
  onRunScript,
}: Props) {
  const online = agents.filter((agent) => agent.connected).length;
  const counts: Record<AgentStatusFilter, number> = { all: agents.length, online, offline: agents.length - online };
  const hasFilters = Boolean(query.trim() || status !== "all");
  const hasData = inventory.status !== "loading";
  const stale = inventory.status === "stale";
  const browserOffline = inventory.connection === "offline";

  return (
    <>
      <HeaderActions>
        <div className="segmented" role="group" aria-label="Show devices">
          {FILTERS.map(({ id, label }) => (
            <button key={id} type="button" aria-pressed={status === id} onClick={() => onStatusChange(id)}>
              {id !== "all" && <i className={`led ${id}`} aria-hidden="true" />}{label}<span>{hasData ? counts[id] : "–"}</span>
            </button>
          ))}
        </div>
        <label className="search-field"><Search size={16} aria-hidden="true" /><input type="search" aria-label="Search devices" value={query} onChange={(event) => onQueryChange(event.target.value)} placeholder="Search devices" /></label>
        <button type="button" className="icon-button" onClick={onRefresh} aria-label="Refresh devices" title="Refresh"><RefreshCw size={16} className={inventory.isRefreshing ? "spin" : ""} /></button>
        {allowed.enroll && <button type="button" className="primary-button" onClick={(event) => onAddDevice(event.currentTarget)} aria-haspopup="dialog"><Plus size={16} /> Add device</button>}
      </HeaderActions>

      <InventoryStrip inventory={inventory} onReconnect={onReconnect} />

      {filteredAgents.length > 0 ? (
        <ul className={`device-wall${stale ? " stale" : ""}`} aria-label="Devices">
          {filteredAgents.map((agent) => (
            <DeviceTile
              key={agent.id}
              agent={agent}
              reading={metrics.get(agent.id)}
              store={thumbnails}
              allowed={allowed}
              state={{
                stale,
                browserOffline,
                connecting: connectingId === agent.id,
                background: connectingBackgroundId === agent.id,
                deleting: deletingId === agent.id,
                closing: closingId === agent.id,
                anyClosing: closingId !== null,
              }}
              onRemote={onRemote}
              onRemoteBackground={onRemoteBackground}
              onCloseSession={onCloseSession}
              onDelete={onDelete}
              onRunScript={onRunScript}
            />
          ))}
        </ul>
      ) : (
        <div className="empty-state">
          {!hasData ? <LoaderCircle size={22} className="spin" aria-hidden="true" /> : <Monitor size={22} aria-hidden="true" />}
          <strong>{!hasData ? "Connecting to your devices…" : hasFilters ? "No matching devices" : "No devices yet"}</strong>
          {hasData && !hasFilters && <span>{allowed.enroll ? "Add a computer to manage it from here." : "Ask an administrator to add one."}</span>}
          {hasFilters && <button className="secondary-button" onClick={() => { onQueryChange(""); onStatusChange("all"); }}>Clear filters</button>}
        </div>
      )}
    </>
  );
}

function DeviceTile({ agent, reading, store, allowed, state, onRemote, onRemoteBackground, onCloseSession, onDelete, onRunScript }: {
  agent: Agent;
  reading: MetricsReading | undefined;
  store: ThumbnailStore;
  allowed: DeviceActions;
  state: TileState;
  onRemote: (agent: Agent) => void;
  onRemoteBackground: (agent: Agent) => void;
  onCloseSession: (agent: Agent) => void;
  onDelete: (agent: Agent) => void;
  onRunScript: (agent: Agent, opener: HTMLElement) => void;
}) {
  const [element, setElement] = useState<HTMLLIElement | null>(null);
  const thumbnail = useDeviceScreen(store, agent, element);
  const [previewOpen, setPreviewOpen] = useState(false);
  const previewOpener = useRef<HTMLElement | null>(null);
  const busy = state.connecting || state.deleting || state.closing;
  const canConnect = allowed.connect && agent.connected && !state.browserOffline && !busy;
  const tone = agent.connected ? "online" : agent.updating_to ? "updating" : "offline";
  // The handoff API checks the device itself, so a stale wall only warns.
  const connectTitle = state.browserOffline ? "You’re offline" : state.stale ? "Status may be out of date" : undefined;

  const openPreview = (opener: HTMLElement) => {
    previewOpener.current = opener;
    setPreviewOpen(true);
  };

  const screen = (
    <>
      {thumbnail ? <img src={thumbnail.url} alt="" /> : <span className="screen-blank">{agent.connected ? <Monitor size={26} aria-hidden="true" /> : <MonitorOff size={26} aria-hidden="true" />}</span>}
      <span className="screen-label" aria-hidden="true">
        {state.connecting ? <><LoaderCircle size={15} className="spin" />{state.background ? "Opening…" : "Connecting…"}</>
          : state.deleting ? <><LoaderCircle size={15} className="spin" />Deleting…</>
          : agent.connected ? <><Monitor size={15} />Connect</>
          : agent.updating_to ? `Updating to ${agent.updating_to}`
          : "Offline"}
      </span>
    </>
  );

  return (
    <li ref={setElement} className={`device-tile ${tone}${busy ? " busy" : ""}`}>
      {canConnect
        ? <button type="button" className="device-screen" onClick={() => onRemote(agent)} title={connectTitle} aria-label={`Connect to ${agent.name}`}>{screen}</button>
        : thumbnail && !busy
          ? <button type="button" className="device-screen" onClick={(event) => openPreview(event.currentTarget)} aria-label={`Show the screen of ${agent.name}`} aria-haspopup="dialog">{screen}</button>
          : <div className="device-screen">{screen}</div>}
      <div className="device-meta">
        <i className={`led ${tone}`} aria-hidden="true" />
        <strong title={agent.name}><Link to={devicePath(agent)}>{agent.name}</Link></strong>
        <span className="sr-only">{tone === "online" ? "Online" : tone === "updating" ? `Updating to ${agent.updating_to}` : "Offline"}</span>
        <DeviceMenu
          agent={agent}
          allowed={allowed}
          state={state}
          hasScreen={Boolean(thumbnail)}
          onShowScreen={openPreview}
          onRemoteBackground={onRemoteBackground}
          onCloseSession={onCloseSession}
          onDelete={onDelete}
          onRunScript={onRunScript}
        />
      </div>
      {reading && <DeviceUsage reading={reading} />}
      {previewOpen && thumbnail && <ScreenPreview agent={agent} thumbnail={thumbnail} returnFocus={previewOpener} onClose={() => setPreviewOpen(false)} />}
    </li>
  );
}

function DeviceMenu({ agent, allowed, state, hasScreen, onShowScreen, onRemoteBackground, onCloseSession, onDelete, onRunScript }: {
  agent: Agent;
  allowed: DeviceActions;
  state: TileState;
  hasScreen: boolean;
  onShowScreen: (opener: HTMLElement) => void;
  onRemoteBackground: (agent: Agent) => void;
  onCloseSession: (agent: Agent) => void;
  onDelete: (agent: Agent) => void;
  onRunScript: (agent: Agent, opener: HTMLElement) => void;
}) {
  const { open, setOpen, close, container, trigger } = usePopover();
  const navigate = useNavigate();
  const menuId = `device-menu-${agent.id}`;
  const reachable = agent.connected && !state.browserOffline;
  // A chosen item's dialog gives focus back to the menu's trigger.
  const choose = (action: (opener: HTMLElement) => void) => {
    close();
    if (trigger.current) action(trigger.current);
  };

  return (
    <div className="popover-anchor device-menu" ref={container}>
      <button ref={trigger} type="button" className="icon-button ghost" onClick={() => setOpen(!open)} aria-expanded={open} aria-controls={open ? menuId : undefined} aria-label={`More actions for ${agent.name}`}>
        <Ellipsis size={18} />
      </button>
      {open && (
        <div id={menuId} className="popover menu" role="group" aria-label={`Actions for ${agent.name}`}>
          {allowed.connectBackground && <button type="button" disabled={!reachable || state.connecting} onClick={() => choose(() => onRemoteBackground(agent))}><Layers size={16} aria-hidden="true" />Connect in background</button>}
          {allowed.runScripts && <button type="button" disabled={!reachable || state.deleting} onClick={() => choose((opener) => onRunScript(agent, opener))} aria-haspopup="dialog"><SquareTerminal size={16} aria-hidden="true" />Run a script…</button>}
          <button type="button" onClick={() => { close(); void navigate(devicePath(agent)); }}><Activity size={16} aria-hidden="true" />Resource usage</button>
          {hasScreen && <button type="button" onClick={() => choose(onShowScreen)} aria-haspopup="dialog"><Expand size={16} aria-hidden="true" />View screen</button>}
          {allowed.closeSession && <button type="button" disabled={state.anyClosing || state.connecting || state.deleting} onClick={() => choose(() => onCloseSession(agent))}><Square size={16} aria-hidden="true" />Close active session</button>}
          <button type="button" onClick={() => { void navigator.clipboard?.writeText(agent.id); close(); }}><Copy size={16} aria-hidden="true" />Copy device ID</button>
          {allowed.delete && <button type="button" className="danger" disabled={state.deleting || state.closing} onClick={() => choose(() => onDelete(agent))}><Trash2 size={16} aria-hidden="true" />Delete device</button>}
        </div>
      )}
    </div>
  );
}
