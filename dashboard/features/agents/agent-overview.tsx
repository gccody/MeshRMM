import { type KeyboardEvent, type ReactNode, memo, useCallback, useEffect, useMemo, useRef, useState } from "react";
import { Link } from "react-router";
import {
  Building2,
  Check,
  ChevronDown,
  Copy,
  Ellipsis,
  Expand,
  FolderOpen,
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
  Tag as TagIcon,
  Tags as TagsIcon,
  Terminal,
  Trash2,
  WifiOff,
} from "lucide-react";
import { usePopover } from "../../lib/use-popover";
import { useGridRows, useWindowedRows } from "../../lib/use-windowed-rows";
import { terminalPagePath } from "../terminal/model";
import { filesPagePath } from "../files/model";
import { DeviceUsage } from "../metrics/usage-meter";
import { HeaderActions } from "../workspace/header-actions";
import { type TagMatch, matchesTagName, tagLookup } from "../tags/model";
import { TagChip, TagChips } from "../tags/tag-chip";
import { type AgentStatusFilter, type CompanyFilter, NO_COMPANY } from "./device-filters";
import { chosenAgents, companyName, matchesCompanyName } from "./model";
import type { DeviceStore } from "./device-store";
import { ScreenPreview, useDeviceScreen } from "./device-thumbnail";
import type { InventoryConnection, InventoryStatus } from "./inventory-stream";
import type { ThumbnailStore } from "./thumbnails";
import { type Agent, type Company, type Tag, hasBackgroundDesktop } from "./types";
import { useDeviceCounts, useDeviceLookup, useDeviceReading, useDevicesUpdated } from "./use-devices";

type InventoryState = {
  devices: DeviceStore;
  thumbnails: ThumbnailStore;
  status: InventoryStatus;
  connection: InventoryConnection;
  error: string | null;
  isRefreshing: boolean;
};

type Props = {
  filteredAgents: readonly Agent[];
  inventory: InventoryState;
  onReconnect: () => void;
  onRefresh: () => void;
  onAddDevice: (opener: HTMLElement) => void;
  companies: readonly Company[];
  tags: readonly Tag[];
  query: string;
  status: AgentStatusFilter;
  company: CompanyFilter;
  // The tags the wall shows devices with, and whether they need any or all.
  tagFilter: readonly string[];
  tagMatch: TagMatch;
  connectingId: string | null;
  connectingBackgroundId: string | null;
  deletingId: string | null;
  closingId: string | null;
  allowed: DeviceActions;
  onQueryChange: (query: string) => void;
  onStatusChange: (status: AgentStatusFilter) => void;
  onCompanyChange: (company: CompanyFilter) => void;
  onTagsChange: (tags: string[], match: TagMatch) => void;
  onClearFilters: () => void;
  // Adds and removes tags on one device or many.
  onEditTags: (agents: Agent[], opener: HTMLElement) => void;
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
  onSelectAll: (agents: readonly Agent[]) => void;
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
  // Browse the device's files.
  files: boolean;
  // Wake offline devices with Wake-on-LAN.
  wake: boolean;
  deliverFiles: boolean;
  delete: boolean;
  enroll: boolean;
  moveCompany: boolean;
  // Add and remove devices' tags.
  editTags: boolean;
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

// A tile's height and the gap below it before the first is measured.
const TILE_ESTIMATE = 300;

// The device's own page, with its resource usage.
const devicePath = (agent: Agent) => `/device?id=${encodeURIComponent(agent.id)}`;

const formatTime = (date: Date | null) =>
  date?.toLocaleTimeString([], { hour: "numeric", minute: "2-digit" }) ?? "";

// Explains why the wall may be out of date and offers to reconnect.
function InventoryStrip({ inventory, onReconnect }: { inventory: InventoryState; onReconnect: () => void }) {
  const { status, connection, error } = inventory;
  const lastUpdated = useDevicesUpdated(inventory.devices);
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
  filteredAgents,
  inventory,
  onReconnect,
  onRefresh,
  onAddDevice,
  companies,
  tags,
  query,
  status,
  company,
  tagFilter,
  tagMatch,
  connectingId,
  connectingBackgroundId,
  deletingId,
  closingId,
  allowed,
  onQueryChange,
  onStatusChange,
  onCompanyChange,
  onTagsChange,
  onClearFilters,
  onEditTags,
  onMove,
  onRemote,
  onRemoteBackground,
  onCloseSession,
  onDelete,
  onRunScript,
  onWake,
  selection,
}: Props) {
  const { devices, thumbnails } = inventory;
  const { total, online } = useDeviceCounts(devices);
  const counts: Record<AgentStatusFilter, number> = { all: total, online, offline: total - online };
  const hasFilters = Boolean(query.trim() || status !== "all" || company || tagFilter.length);
  const hasData = inventory.status !== "loading";
  const stale = inventory.status === "stale";
  const browserOffline = inventory.connection === "offline";
  const selectable = allowed.runScripts || allowed.deliverFiles || allowed.wake || allowed.editTags;
  // The chosen devices still in the inventory, found by ID.
  const fleet = useDeviceLookup(devices);
  const chosen = useMemo(() => chosenAgents(selection.ids, fleet), [fleet, selection.ids]);
  const selectedCount = chosen.length;
  // Looked up by every tile drawn, so built once, not once a tile.
  const companyNames = useMemo(() => new Map(companies.map((candidate) => [candidate.id, candidate.name])), [companies]);
  const tagsFor = useMemo(() => tagLookup(tags), [tags]);
  // The device whose screen is shown full size, and the menu that asked.
  // It stays open while its tile scrolls away, and closes if its device
  // leaves the wall.
  const [previewId, setPreviewId] = useState<string | null>(null);
  const previewOpener = useRef<HTMLElement | null>(null);
  const previewed = previewId === null ? undefined : filteredAgents.find((agent) => agent.id === previewId);
  if (previewId !== null && !previewed) setPreviewId(null);
  const onShowScreen = useCallback((agent: Agent, opener: HTMLElement) => {
    previewOpener.current = opener;
    setPreviewId(agent.id);
  }, []);

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
        {(tags.length > 0 || tagFilter.length > 0) && <TagFilterMenu tags={tags} chosen={tagFilter} match={tagMatch} onChange={onTagsChange} />}
        <label className="search-field"><Search size={16} aria-hidden="true" /><input type="search" aria-label="Search devices" value={query} onChange={(event) => onQueryChange(event.target.value)} placeholder="Search devices" title="Searches names, tags, operating systems, models, serial numbers and IP addresses" /></label>
        <button type="button" className="icon-button" onClick={onRefresh} aria-label="Refresh devices" title="Refresh"><RefreshCw size={16} className={inventory.isRefreshing ? "spin" : ""} /></button>
        {allowed.enroll && <button type="button" className="primary-button" onClick={(event) => onAddDevice(event.currentTarget)} aria-haspopup="dialog"><Plus size={16} /> Add device</button>}
      </HeaderActions>

      <InventoryStrip inventory={inventory} onReconnect={onReconnect} />
      {selectable && selectedCount > 0 && <SelectionBar count={selectedCount} allowed={allowed} selection={selection} shown={filteredAgents} chosen={chosen} onWake={onWake} onEditTags={onEditTags} />}

      {filteredAgents.length > 0 ? (
        <DeviceWall agents={filteredAgents} className={`device-wall${stale ? " stale" : ""}${selectedCount > 0 ? " selecting" : ""}`}>
          {(agent, index) => (
            <DeviceTile
              key={agent.id}
              agent={agent}
              position={index + 1}
              setSize={filteredAgents.length}
              companyName={(agent.company_id && companyNames.get(agent.company_id)) || null}
              tagsFor={tagsFor}
              devices={devices}
              thumbnails={thumbnails}
              allowed={allowed}
              selected={selectable ? selection.ids.has(agent.id) : null}
              onSelect={selection.onChange}
              stale={stale}
              browserOffline={browserOffline}
              connecting={connectingId === agent.id}
              background={connectingBackgroundId === agent.id}
              deleting={deletingId === agent.id}
              closing={closingId === agent.id}
              anyClosing={closingId !== null}
              onRemote={onRemote}
              onRemoteBackground={onRemoteBackground}
              onCloseSession={onCloseSession}
              onDelete={onDelete}
              onRunScript={onRunScript}
              onWake={onWake}
              onMove={onMove}
              onEditTags={onEditTags}
              onShowScreen={onShowScreen}
            />
          )}
        </DeviceWall>
      ) : (
        <div className="empty-state">
          {!hasData ? <LoaderCircle size={22} className="spin" aria-hidden="true" /> : <Monitor size={22} aria-hidden="true" />}
          <strong>{!hasData ? "Connecting to your devices…" : hasFilters ? "No matching devices" : "No devices yet"}</strong>
          {hasData && !hasFilters && <span>{allowed.enroll ? "Add a computer to manage it from here." : "Ask an administrator to add one."}</span>}
          {hasFilters && <button className="secondary-button" onClick={onClearFilters}>Clear filters</button>}
        </div>
      )}
      {previewed && <ScreenPreview agent={previewed} store={thumbnails} returnFocus={previewOpener} onClose={() => setPreviewId(null)} />}
    </>
  );
}

// The devices as a grid of screens, as many to a row as fit. It draws only
// the rows on screen and one either side, so a fleet of a hundred thousand
// scrolls as easily as a few; the list's padding stands in for the rest.
// Every tile is the same height, so every row is.
function DeviceWall({ agents, className, children }: {
  agents: readonly Agent[];
  className: string;
  children: (agent: Agent, index: number) => ReactNode;
}) {
  const list = useRef<HTMLUListElement>(null);
  const { columns, rowHeight } = useGridRows(list, TILE_ESTIMATE);
  const { first, last } = useWindowedRows({ list }, agents.length, rowHeight, { columns, overscan: 1 });
  return (
    <ul ref={list} className={className} aria-label="Devices">
      {agents.slice(first, last).map((agent, offset) => children(agent, first + offset))}
    </ul>
  );
}

// What to do with the devices chosen on the wall.
function SelectionBar({ count, allowed, selection, shown, chosen, onWake, onEditTags }: {
  count: number;
  allowed: DeviceActions;
  selection: DeviceSelection;
  shown: readonly Agent[];
  // The chosen devices still in the inventory.
  chosen: Agent[];
  onWake: (agents: Agent[], opener: HTMLElement) => void;
  onEditTags: (agents: Agent[], opener: HTMLElement) => void;
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
        {allowed.editTags && <button type="button" className="secondary-button" onClick={(event) => onEditTags(chosen, event.currentTarget)} aria-haspopup="dialog"><TagIcon size={16} aria-hidden="true" />Tag</button>}
      </span>
    </div>
  );
}

// Shows every device, one company's, or those in none.
function CompanyFilterMenu({ companies, company, onChange }: {
  companies: readonly Company[];
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
  companies: readonly Company[];
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

// The tags label: "All tags", one tag's name, or the first and how many more.
function tagFilterLabel(tags: readonly Tag[], chosen: readonly string[]) {
  if (!chosen.length) return "All tags";
  const first = tags.find((tag) => tag.id === chosen[0])?.name ?? "Unknown tag";
  return chosen.length === 1 ? first : `${first} +${chosen.length - 1}`;
}

// Shows the devices with any, or all, of chosen tags.
function TagFilterMenu({ tags, chosen, match, onChange }: {
  tags: readonly Tag[];
  chosen: readonly string[];
  match: TagMatch;
  onChange: (tags: string[], match: TagMatch) => void;
}) {
  const { open, setOpen, container, trigger } = usePopover();
  const label = tagFilterLabel(tags, chosen);
  return (
    <div className="popover-anchor" ref={container}>
      <button ref={trigger} type="button" className="company-filter" onClick={() => setOpen(!open)} aria-expanded={open} aria-controls={open ? "tag-filter-menu" : undefined} aria-label={`Tags: ${label}`}>
        <TagsIcon size={15} aria-hidden="true" /><span>{label}</span><ChevronDown size={14} aria-hidden="true" />
      </button>
      {open && <TagFilterList tags={tags} chosen={chosen} match={match} onChange={onChange} />}
    </div>
  );
}

// The open tags menu: tick several, and whether devices need any or all.
// It stays open while choosing, and its search box starts empty each time.
function TagFilterList({ tags, chosen, match, onChange }: {
  tags: readonly Tag[];
  chosen: readonly string[];
  match: TagMatch;
  onChange: (tags: string[], match: TagMatch) => void;
}) {
  const [query, setQuery] = useState("");
  const search = useRef<HTMLInputElement>(null);
  useEffect(() => search.current?.focus(), []);
  const shown = tags.filter((tag) => matchesTagName(tag.name, query));
  // A link may name tags this browser doesn't know (yet); they stay chosen.
  const unknown = chosen.filter((id) => !tags.some((tag) => tag.id === id));
  const toggle = (id: string) => onChange(chosen.includes(id) ? chosen.filter((candidate) => candidate !== id) : [...chosen, id], match);

  return (
    <div id="tag-filter-menu" className="popover menu company-filter-menu tag-filter-menu" role="group" aria-label="Show devices with tags">
      <label className="menu-search"><Search size={15} aria-hidden="true" />
        <input ref={search} type="search" aria-label="Search tags" placeholder="Search tags" value={query} onChange={(event) => setQuery(event.target.value)} />
      </label>
      {chosen.length > 1 && (
        <div className="segmented tag-match" role="group" aria-label="Devices need">
          <button type="button" aria-pressed={match === "any"} onClick={() => onChange([...chosen], "any")}>Any of these</button>
          <button type="button" aria-pressed={match === "all"} onClick={() => onChange([...chosen], "all")}>All of them</button>
        </div>
      )}
      <div className="menu-options">
        {shown.map((tag) => (
          <button key={tag.id} type="button" aria-pressed={chosen.includes(tag.id)} onClick={() => toggle(tag.id)}>
            <Check size={16} aria-hidden="true" className={chosen.includes(tag.id) ? undefined : "invisible"} /><TagChip tag={tag} />
          </button>
        ))}
        {unknown.length > 0 && !query.trim() && (
          <button type="button" aria-pressed="true" onClick={() => onChange(chosen.filter((id) => !unknown.includes(id)), match)}>
            <Check size={16} aria-hidden="true" /><span>{unknown.length === 1 ? "An unknown tag" : `${unknown.length} unknown tags`}</span>
          </button>
        )}
        {shown.length === 0 && <p className="menu-empty" role="status">{tags.length ? "No matching tags" : "No tags yet"}</p>}
      </div>
      {chosen.length > 0 && <button type="button" className="link-button tag-filter-clear" onClick={() => onChange([], "any")}>Clear tags</button>}
    </div>
  );
}

// Memoised, with props that keep their identity: a device's change, or its
// reading, redraws its own tile and no other.
const DeviceTile = memo(function DeviceTile({ agent, position, setSize, companyName, tagsFor, devices, thumbnails, allowed, selected, onSelect, stale, browserOffline, connecting, background, deleting, closing, anyClosing, onRemote, onRemoteBackground, onCloseSession, onDelete, onRunScript, onWake, onMove, onEditTags, onShowScreen }: {
  agent: Agent;
  // Where it is on the wall, which draws only some of its tiles.
  position: number;
  setSize: number;
  companyName: string | null;
  tagsFor: (ids: readonly string[] | undefined) => Tag[];
  // Where the tile reads its device's latest resource usage.
  devices: DeviceStore;
  thumbnails: ThumbnailStore;
  allowed: DeviceActions;
  // Whether it's chosen for a job; null when the user can't start jobs.
  selected: boolean | null;
  onSelect: (agent: Agent, selected: boolean) => void;
  stale: boolean;
  browserOffline: boolean;
  connecting: boolean;
  background: boolean;
  deleting: boolean;
  closing: boolean;
  anyClosing: boolean;
  onRemote: (agent: Agent) => void;
  onRemoteBackground: (agent: Agent) => void;
  onCloseSession: (agent: Agent) => void;
  onDelete: (agent: Agent) => void;
  onRunScript: (agent: Agent, opener: HTMLElement) => void;
  onWake: (agents: Agent[], opener: HTMLElement) => void;
  onMove: (agent: Agent, opener: HTMLElement) => void;
  onEditTags: (agents: Agent[], opener: HTMLElement) => void;
  onShowScreen: (agent: Agent, opener: HTMLElement) => void;
}) {
  const thumbnail = useDeviceScreen(thumbnails, agent);
  const reading = useDeviceReading(devices, agent.id);
  const tags = useMemo(() => tagsFor(agent.tag_ids), [tagsFor, agent.tag_ids]);
  const state: TileState = { stale, browserOffline, connecting, background, deleting, closing, anyClosing };
  const showScreen = (opener: HTMLElement) => onShowScreen(agent, opener);
  const busy = state.connecting || state.deleting || state.closing;
  const reachable = agent.connected && !state.browserOffline && !busy;
  const connectActions = hasBackgroundDesktop(agent) ? allowed : { ...allowed, connectBackground: false };
  const canConnect = reachable && (connectActions.connect || connectActions.connectBackground);
  const canRunScripts = reachable && allowed.runScripts;
  const canOpenTerminal = reachable && allowed.terminal;
  const tone = agent.connected ? "online" : agent.updating_to ? "updating" : "offline";

  // The device's name links to the same page, so the screen is for pointers only.
  return (
    <li className={`device-tile ${tone}${busy ? " busy" : ""}${selected ? " selected" : ""}`} aria-posinset={position} aria-setsize={setSize}>
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
      <div className="device-details">
        <div className="device-meta">
          <i className={`led ${tone}`} aria-hidden="true" />
          <strong title={agent.name}><Link to={devicePath(agent)}>{agent.name}</Link></strong>
          <span className="sr-only">{tone === "online" ? "Online" : tone === "updating" ? `Updating to ${agent.updating_to}` : "Offline"}</span>
          <DeviceMenu
            agent={agent}
            allowed={allowed}
            state={state}
            hasScreen={Boolean(thumbnail)}
            onShowScreen={showScreen}
            onCloseSession={onCloseSession}
            onDelete={onDelete}
            onWake={onWake}
            onMove={onMove}
            onEditTags={onEditTags}
          />
        </div>
        {agent.system?.os && <p className="device-company" title={agent.system.os}><Laptop size={13} aria-hidden="true" /><span><span className="sr-only">Operating system: </span>{agent.system.os}</span></p>}
        {companyName && <p className="device-company" title={companyName}><Building2 size={13} aria-hidden="true" /><span><span className="sr-only">Company: </span>{companyName}</span></p>}
        {tags.length > 0 && <div className="device-tags"><TagChips tags={tags} limit={3} /></div>}
        {reading && <DeviceUsage reading={reading} />}
      </div>
    </li>
  );
});

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

function DeviceMenu({ agent, allowed, state, hasScreen, onShowScreen, onCloseSession, onDelete, onWake, onMove, onEditTags }: {
  agent: Agent;
  allowed: DeviceActions;
  state: TileState;
  hasScreen: boolean;
  onShowScreen: (opener: HTMLElement) => void;
  onCloseSession: (agent: Agent) => void;
  onDelete: (agent: Agent) => void;
  onWake: (agents: Agent[], opener: HTMLElement) => void;
  onMove: (agent: Agent, opener: HTMLElement) => void;
  onEditTags: (agents: Agent[], opener: HTMLElement) => void;
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
          {allowed.files && agent.connected && <a href={filesPagePath(agent.id)} target="_blank" rel="noreferrer" onClick={close}><FolderOpen size={16} aria-hidden="true" />Browse files</a>}
          {allowed.closeSession && <button type="button" disabled={state.anyClosing || state.connecting || state.deleting} onClick={() => choose(() => onCloseSession(agent))}><Square size={16} aria-hidden="true" />Close active session</button>}
          {allowed.moveCompany && <button type="button" disabled={state.deleting} onClick={() => choose((opener) => onMove(agent, opener))} aria-haspopup="dialog"><Building2 size={16} aria-hidden="true" />Move to company…</button>}
          {allowed.editTags && <button type="button" disabled={state.deleting} onClick={() => choose((opener) => onEditTags([agent], opener))} aria-haspopup="dialog"><TagIcon size={16} aria-hidden="true" />Tags…</button>}
          <button type="button" onClick={() => { void navigator.clipboard?.writeText(agent.id); close(); }}><Copy size={16} aria-hidden="true" />Copy device ID</button>
          {allowed.delete && <button type="button" className="danger" disabled={state.deleting || state.closing} onClick={() => choose(() => onDelete(agent))}><Trash2 size={16} aria-hidden="true" />Delete device</button>}
        </div>
      )}
    </div>
  );
}
