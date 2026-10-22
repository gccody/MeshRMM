import { type KeyboardEvent, useEffect, useRef, useState } from "react";
import { Link } from "react-router";
import {
  Building2,
  Check,
  ChevronDown,
  Copy,
  Ellipsis,
  Expand,
  Laptop,
  Layers,
  LoaderCircle,
  Monitor,
  MonitorOff,
  Plus,
  Power,
  RefreshCw,
  Search,
  Send,
  Square,
  SquareTerminal,
  Terminal,
  Trash2,
  WifiOff,
} from "lucide-react";
import { usePopover } from "../../lib/use-popover";
import { terminalPagePath } from "../terminal/model";
import type { MetricsReading } from "../metrics/model";
import { DeviceUsage } from "../metrics/usage-meter";
import { HeaderActions } from "../workspace/header-actions";
import { type AgentStatusFilter, type CompanyFilter, NO_COMPANY } from "./device-filters";
import { companyName, matchesCompanyName } from "./model";
import { ScreenPreview, useDeviceScreen } from "./device-thumbnail";
import type { InventoryConnection, InventoryStatus } from "./inventory-stream";
import type { ThumbnailStore } from "./thumbnails";
import { type Agent, type Company, hasBackgroundDesktop } from "./types";

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
  companies: Company[];
  query: string;
  status: AgentStatusFilter;
  company: CompanyFilter;
  connectingId: string | null;
  connectingBackgroundId: string | null;
  deletingId: string | null;
  closingId: string | null;
  allowed: DeviceActions;
  onQueryChange: (query: string) => void;
  onStatusChange: (status: AgentStatusFilter) => void;
  onCompanyChange: (company: CompanyFilter) => void;
  onClearFilters: () => void;
  onMove: (agent: Agent, opener: HTMLElement) => void;
  onRemote: (agent: Agent) => void;
  onRemoteBackground: (agent: Agent) => void;
  onCloseSession: (agent: Agent) => void;
  onDelete: (agent: Agent) => void;
  onRunScript: (agent: Agent, opener: HTMLElement) => void;
  // Wakes offline devices.
  onWake: (agents: Agent[], opener: HTMLElement) => void;
  // The devices chosen for a job, and what to do with them.
  selection: DeviceSelection;
};

// Devices chosen on the wall for a job. `ids` may name devices since
// removed; only those still on the wall count.
export type DeviceSelection = {
  ids: ReadonlySet<string>;
  onChange: (agent: Agent, selected: boolean) => void;
  onSelectAll: (agents: Agent[]) => void;
  onClear: () => void;
  onStartJob: (kind: "script" | "file", opener: HTMLElement) => void;
};

// What the signed-in user may do with a device.
export type DeviceActions = {
  connect: boolean;
  connectBackground: boolean;
  closeSession: boolean;
  runScripts: boolean;
  // Open a terminal as either account.
  terminal: boolean;
  // Wake offline devices with Wake-on-LAN.
  wake: boolean;
  deliverFiles: boolean;
  delete: boolean;
  enroll: boolean;
  moveCompany: boolean;
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
  companies,
  query,
  status,
  company,
  connectingId,
  connectingBackgroundId,
  deletingId,
  closingId,
  allowed,
  onQueryChange,
  onStatusChange,
  onCompanyChange,
  onClearFilters,
  onMove,
  onRemote,
  onRemoteBackground,
  onCloseSession,
  onDelete,
  onRunScript,
  onWake,
  selection,
}: Props) {
  const online = agents.filter((agent) => agent.connected).length;
  const counts: Record<AgentStatusFilter, number> = { all: agents.length, online, offline: agents.length - online };
  const hasFilters = Boolean(query.trim() || status !== "all" || company);
  const hasData = inventory.status !== "loading";
  const stale = inventory.status === "stale";
  const browserOffline = inventory.connection === "offline";
  const selectable = allowed.runScripts || allowed.deliverFiles || allowed.wake;
  const selectedCount = agents.filter((agent) => selection.ids.has(agent.id)).length;

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
        {(companies.length > 0 || company) && <CompanyFilterMenu companies={companies} company={company} onChange={onCompanyChange} />}
        <label className="search-field"><Search size={16} aria-hidden="true" /><input type="search" aria-label="Search devices" value={query} onChange={(event) => onQueryChange(event.target.value)} placeholder="Search devices" title="Searches names, operating systems, models, serial numbers and IP addresses" /></label>
        <button type="button" className="icon-button" onClick={onRefresh} aria-label="Refresh devices" title="Refresh"><RefreshCw size={16} className={inventory.isRefreshing ? "spin" : ""} /></button>
        {allowed.enroll && <button type="button" className="primary-button" onClick={(event) => onAddDevice(event.currentTarget)} aria-haspopup="dialog"><Plus size={16} /> Add device</button>}
      </HeaderActions>

      <InventoryStrip inventory={inventory} onReconnect={onReconnect} />
      {selectable && selectedCount > 0 && <SelectionBar count={selectedCount} allowed={allowed} selection={selection} shown={filteredAgents} chosen={agents.filter((agent) => selection.ids.has(agent.id))} onWake={onWake} />}

      {filteredAgents.length > 0 ? (
        <ul className={`device-wall${stale ? " stale" : ""}${selectedCount > 0 ? " selecting" : ""}`} aria-label="Devices">
          {filteredAgents.map((agent) => (
            <DeviceTile
              key={agent.id}
              agent={agent}
              companyName={companyName(companies, agent.company_id)}
              reading={metrics.get(agent.id)}
              store={thumbnails}
              allowed={allowed}
              selected={selectable ? selection.ids.has(agent.id) : null}
              onSelect={selection.onChange}
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
              onWake={onWake}
              onMove={onMove}
            />
          ))}
        </ul>
      ) : (
        <div className="empty-state">
          {!hasData ? <LoaderCircle size={22} className="spin" aria-hidden="true" /> : <Monitor size={22} aria-hidden="true" />}
          <strong>{!hasData ? "Connecting to your devices…" : hasFilters ? "No matching devices" : "No devices yet"}</strong>
          {hasData && !hasFilters && <span>{allowed.enroll ? "Add a computer to manage it from here." : "Ask an administrator to add one."}</span>}
          {hasFilters && <button className="secondary-button" onClick={onClearFilters}>Clear filters</button>}
        </div>
      )}
    </>
  );
}

// What to do with the devices chosen on the wall.
function SelectionBar({ count, allowed, selection, shown, chosen, onWake }: {
  count: number;
  allowed: DeviceActions;
  selection: DeviceSelection;
  shown: Agent[];
  // The chosen devices still on the wall.
  chosen: Agent[];
  onWake: (agents: Agent[], opener: HTMLElement) => void;
}) {
  const allShown = shown.every((agent) => selection.ids.has(agent.id));
  const offline = chosen.filter((agent) => !agent.connected);
  return (
    <div className="selection-bar" role="region" aria-label="Chosen devices">
      <strong role="status">{count} {count === 1 ? "device" : "devices"} selected</strong>
      {!allShown && <button type="button" className="link-button" onClick={() => selection.onSelectAll(shown)}>Select all {shown.length} shown</button>}
      <button type="button" className="link-button" onClick={selection.onClear}>Clear</button>
      <span className="selection-actions">
        {allowed.runScripts && <button type="button" className="primary-button" onClick={(event) => selection.onStartJob("script", event.currentTarget)} aria-haspopup="dialog"><SquareTerminal size={16} aria-hidden="true" />Run script</button>}
        {allowed.deliverFiles && <button type="button" className="secondary-button" onClick={(event) => selection.onStartJob("file", event.currentTarget)} aria-haspopup="dialog"><Send size={16} aria-hidden="true" />Send file</button>}
        {allowed.wake && offline.length > 0 && <button type="button" className="secondary-button" onClick={(event) => onWake(offline, event.currentTarget)} aria-haspopup="dialog"><Power size={16} aria-hidden="true" />Wake {offline.length} offline</button>}
      </span>
    </div>
  );
}

// Shows every device, one company's, or those in none.
function CompanyFilterMenu({ companies, company, onChange }: {
  companies: Company[];
  company: CompanyFilter;
  onChange: (company: CompanyFilter) => void;
}) {
  const { open, setOpen, close, container, trigger } = usePopover();
  // A link may name a company this browser doesn't know (yet).
  const label = company === "" ? "All companies"
    : company === NO_COMPANY ? "No company"
    : companyName(companies, company) ?? "Unknown company";
  const choose = (id: CompanyFilter) => {
    close();
    onChange(id);
  };

  return (
    <div className="popover-anchor" ref={container}>
      <button ref={trigger} type="button" className="company-filter" onClick={() => setOpen(!open)} aria-expanded={open} aria-controls={open ? "company-filter-menu" : undefined} aria-label={`Company: ${label}`}>
        <Building2 size={15} aria-hidden="true" /><span>{label}</span><ChevronDown size={14} aria-hidden="true" />
      </button>
      {open && <CompanyFilterList companies={companies} company={company} onChoose={choose} />}
    </div>
  );
}

// The open company menu, with a search box that starts empty each time.
function CompanyFilterList({ companies, company, onChoose }: {
  companies: Company[];
  company: CompanyFilter;
  onChoose: (company: CompanyFilter) => void;
}) {
  const [query, setQuery] = useState("");
  const list = useRef<HTMLDivElement>(null);
  const search = useRef<HTMLInputElement>(null);
  useEffect(() => search.current?.focus(), []);
  const searching = Boolean(query.trim());
  // While searching, only the companies that match.
  const options = searching
    ? companies.filter((candidate) => matchesCompanyName(candidate.name, query)).map((candidate) => ({ id: candidate.id, label: candidate.name }))
    : [
      { id: "", label: "All companies" },
      ...companies.map((candidate) => ({ id: candidate.id, label: candidate.name })),
      { id: NO_COMPANY, label: "No company" },
    ];

  // The arrow keys move between the search box and the options.
  const moveFocus = (event: KeyboardEvent<HTMLElement>) => {
    if (event.key !== "ArrowDown" && event.key !== "ArrowUp") return;
    const items = [search.current, ...(list.current?.querySelectorAll<HTMLButtonElement>("button") ?? [])];
    const index = items.indexOf(document.activeElement as HTMLInputElement | HTMLButtonElement);
    const next = items[Math.min(items.length - 1, Math.max(0, index + (event.key === "ArrowDown" ? 1 : -1)))];
    if (!next) return;
    event.preventDefault();
    next.focus();
  };

  return (
    <div id="company-filter-menu" className="popover menu company-filter-menu" role="group" aria-label="Show devices in">
      <label className="menu-search"><Search size={15} aria-hidden="true" />
        <input
          ref={search}
          type="search"
          aria-label="Search companies"
          placeholder="Search companies"
          value={query}
          onChange={(event) => setQuery(event.target.value)}
          onKeyDown={(event) => {
            if (event.key === "Enter" && searching && options[0]) onChoose(options[0].id);
            else moveFocus(event);
          }}
        />
      </label>
      <div ref={list} className="menu-options">
        {options.map((option) => (
          <button key={option.id || "all"} type="button" aria-pressed={option.id === company} onClick={() => onChoose(option.id)} onKeyDown={moveFocus}>
            <Check size={16} aria-hidden="true" className={option.id === company ? undefined : "invisible"} /><span>{option.label}</span>
          </button>
        ))}
        {options.length === 0 && <p className="menu-empty" role="status">No matching companies</p>}
      </div>
    </div>
  );
}

function DeviceTile({ agent, companyName, reading, store, allowed, selected, onSelect, state, onRemote, onRemoteBackground, onCloseSession, onDelete, onRunScript, onWake, onMove }: {
  agent: Agent;
  companyName: string | null;
  reading: MetricsReading | undefined;
  store: ThumbnailStore;
  allowed: DeviceActions;
  // Whether it's chosen for a job; null when the user can't start jobs.
  selected: boolean | null;
  onSelect: (agent: Agent, selected: boolean) => void;
  state: TileState;
  onRemote: (agent: Agent) => void;
  onRemoteBackground: (agent: Agent) => void;
  onCloseSession: (agent: Agent) => void;
  onDelete: (agent: Agent) => void;
  onRunScript: (agent: Agent, opener: HTMLElement) => void;
  onWake: (agents: Agent[], opener: HTMLElement) => void;
  onMove: (agent: Agent, opener: HTMLElement) => void;
}) {
  const [element, setElement] = useState<HTMLLIElement | null>(null);
  const thumbnail = useDeviceScreen(store, agent, element);
  const [previewOpen, setPreviewOpen] = useState(false);
  const previewOpener = useRef<HTMLElement | null>(null);
  const busy = state.connecting || state.deleting || state.closing;
  const reachable = agent.connected && !state.browserOffline && !busy;
  const connectActions = hasBackgroundDesktop(agent) ? allowed : { ...allowed, connectBackground: false };
  const canConnect = reachable && (connectActions.connect || connectActions.connectBackground);
  const canRunScripts = reachable && allowed.runScripts;
  const canOpenTerminal = reachable && allowed.terminal;
  const tone = agent.connected ? "online" : agent.updating_to ? "updating" : "offline";

  const openPreview = (opener: HTMLElement) => {
    previewOpener.current = opener;
    setPreviewOpen(true);
  };

  // The device's name links to the same page, so the screen is for pointers only.
  return (
    <li ref={setElement} className={`device-tile ${tone}${busy ? " busy" : ""}${selected ? " selected" : ""}`}>
      <div className="device-screen">
        {selected !== null && (
          <label className="tile-select" title={selected ? "Chosen for a job" : "Choose for a job"}>
            <input type="checkbox" checked={selected} onChange={(event) => onSelect(agent, event.target.checked)} aria-label={`Select ${agent.name}`} />
          </label>
        )}
        <Link className="screen-link" to={devicePath(agent)} tabIndex={-1} aria-hidden="true">
          {thumbnail ? <img src={thumbnail.url} alt="" /> : <span className="screen-blank">{agent.connected ? <Monitor size={26} /> : <MonitorOff size={26} />}</span>}
          {(busy || !agent.connected) && (
            <span className="screen-label">
              {state.connecting ? <><LoaderCircle size={15} className="spin" />{state.background ? "Opening…" : "Connecting…"}</>
                : state.deleting ? <><LoaderCircle size={15} className="spin" />Deleting…</>
                : state.closing ? <><LoaderCircle size={15} className="spin" />Closing…</>
                : agent.updating_to ? `Updating to ${agent.updating_to}`
                : "Offline"}
            </span>
          )}
        </Link>
        {(canConnect || canRunScripts || canOpenTerminal) && (
          <div className="screen-actions">
            {canConnect && <ConnectButton agent={agent} allowed={connectActions} stale={state.stale} onRemote={onRemote} onRemoteBackground={onRemoteBackground} />}
            {canRunScripts && <button type="button" className="screen-action" onClick={(event) => onRunScript(agent, event.currentTarget)} aria-haspopup="dialog" aria-label={`Run a script on ${agent.name}`}><SquareTerminal size={15} aria-hidden="true" />Scripts</button>}
            {canOpenTerminal && <TerminalLink agent={agent} />}
          </div>
        )}
      </div>
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
          onCloseSession={onCloseSession}
          onDelete={onDelete}
          onWake={onWake}
          onMove={onMove}
        />
      </div>
      {agent.system?.os && <p className="device-company" title={agent.system.os}><Laptop size={13} aria-hidden="true" /><span><span className="sr-only">Operating system: </span>{agent.system.os}</span></p>}
      {companyName && <p className="device-company" title={companyName}><Building2 size={13} aria-hidden="true" /><span><span className="sr-only">Company: </span>{companyName}</span></p>}
      {reading && <DeviceUsage reading={reading} />}
      {previewOpen && thumbnail && <ScreenPreview agent={agent} thumbnail={thumbnail} returnFocus={previewOpener} onClose={() => setPreviewOpen(false)} />}
    </li>
  );
}

// Terminals open in a tab of their own, so several devices' can stay open
// side by side while the devices list stays here.
function TerminalLink({ agent }: { agent: Agent }) {
  return (
    <a className="screen-action" href={terminalPagePath(agent.id)} target="_blank" rel="noreferrer" aria-label={`Open a terminal on ${agent.name} in a new tab`}>
      <Terminal size={15} aria-hidden="true" />Terminal
    </a>
  );
}

// Connects to the console, or to background mode, whichever the user may
// open and the device has. With both allowed it asks which.
function ConnectButton({ agent, allowed, stale, onRemote, onRemoteBackground }: {
  agent: Agent;
  allowed: DeviceActions;
  stale: boolean;
  onRemote: (agent: Agent) => void;
  onRemoteBackground: (agent: Agent) => void;
}) {
  const { open, setOpen, close, container, trigger } = usePopover();
  const menuId = `connect-menu-${agent.id}`;
  // The handoff API checks the device itself, so a stale wall only warns.
  const title = stale ? "Status may be out of date" : undefined;

  if (!allowed.connect || !allowed.connectBackground) {
    const background = !allowed.connect;
    return (
      <button type="button" className="screen-action" title={title} onClick={() => (background ? onRemoteBackground : onRemote)(agent)} aria-label={background ? `Connect to ${agent.name} in background` : `Connect to ${agent.name}`}>
        {background ? <Layers size={15} aria-hidden="true" /> : <Monitor size={15} aria-hidden="true" />}Connect
      </button>
    );
  }

  return (
    <div className="popover-anchor" ref={container}>
      <button ref={trigger} type="button" className="screen-action" title={title} onClick={() => setOpen(!open)} aria-expanded={open} aria-controls={open ? menuId : undefined} aria-label={`Connect to ${agent.name}`}>
        <Monitor size={15} aria-hidden="true" />Connect<ChevronDown size={14} aria-hidden="true" />
      </button>
      {open && (
        <div id={menuId} className="popover menu" role="group" aria-label={`Connect to ${agent.name}`}>
          <button type="button" onClick={() => { close(); onRemote(agent); }}><Monitor size={16} aria-hidden="true" />Console</button>
          <button type="button" onClick={() => { close(); onRemoteBackground(agent); }}><Layers size={16} aria-hidden="true" />Background</button>
        </div>
      )}
    </div>
  );
}

function DeviceMenu({ agent, allowed, state, hasScreen, onShowScreen, onCloseSession, onDelete, onWake, onMove }: {
  agent: Agent;
  allowed: DeviceActions;
  state: TileState;
  hasScreen: boolean;
  onShowScreen: (opener: HTMLElement) => void;
  onCloseSession: (agent: Agent) => void;
  onDelete: (agent: Agent) => void;
  onWake: (agents: Agent[], opener: HTMLElement) => void;
  onMove: (agent: Agent, opener: HTMLElement) => void;
}) {
  const { open, setOpen, close, container, trigger } = usePopover();
  const menuId = `device-menu-${agent.id}`;
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
          {hasScreen && <button type="button" onClick={() => choose(onShowScreen)} aria-haspopup="dialog"><Expand size={16} aria-hidden="true" />View screen</button>}
          {allowed.wake && !agent.connected && <button type="button" disabled={state.deleting} onClick={() => choose((opener) => onWake([agent], opener))} aria-haspopup="dialog"><Power size={16} aria-hidden="true" />Wake device</button>}
          {allowed.terminal && agent.connected && <a href={terminalPagePath(agent.id)} target="_blank" rel="noreferrer" onClick={close}><Terminal size={16} aria-hidden="true" />Open terminal</a>}
          {allowed.closeSession && <button type="button" disabled={state.anyClosing || state.connecting || state.deleting} onClick={() => choose(() => onCloseSession(agent))}><Square size={16} aria-hidden="true" />Close active session</button>}
          {allowed.moveCompany && <button type="button" disabled={state.deleting} onClick={() => choose((opener) => onMove(agent, opener))} aria-haspopup="dialog"><Building2 size={16} aria-hidden="true" />Move to company…</button>}
          <button type="button" onClick={() => { void navigator.clipboard?.writeText(agent.id); close(); }}><Copy size={16} aria-hidden="true" />Copy device ID</button>
          {allowed.delete && <button type="button" className="danger" disabled={state.deleting || state.closing} onClick={() => choose(() => onDelete(agent))}><Trash2 size={16} aria-hidden="true" />Delete device</button>}
        </div>
      )}
    </div>
  );
}
