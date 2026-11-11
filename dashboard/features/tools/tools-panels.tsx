import {
  ArrowDown,
  ArrowUp,
  ChevronDown,
  ChevronRight,
  CircleAlert,
  Cog,
  ListTree,
  LoaderCircle,
  Pause,
  Play,
  RefreshCw,
  RotateCw,
  Search,
  Square,
  X,
  XCircle,
} from "lucide-react";
import { type FormEvent, type ReactNode, type RefObject, useCallback, useEffect, useId, useMemo, useRef, useState } from "react";
import { AuthenticationRequired, RequestError, errorText } from "../../lib/http";
import { formatDateTime } from "../../lib/format";
import { ModalDialog } from "../../lib/modal-dialog";
import type { Agent, DevicePlatform } from "../agents/types";
import { formatBytes } from "../metrics/model";
import { useWorkspace } from "../workspace/workspace-context";
import {
  ACTION_LABELS,
  DEFAULT_PROCESS_SORT,
  DEFAULT_SERVICE_SORT,
  MAX_ENDED_PROCESSES,
  NUMERIC_PROCESS_KEYS,
  PROCESS_REFRESH_MS,
  type ProcessInfo,
  type ProcessList,
  type ProcessSortKey,
  type ProcessesEnded,
  STATE_LABELS,
  type ServiceAction,
  type ServiceFilter,
  type ServiceInfo,
  type ServiceList,
  type ServiceSortKey,
  type ServiceStartup,
  type Sort,
  errorHelp,
  formatCpu,
  nextSort,
  processMatches,
  processTree,
  processTreeOf,
  serviceActions,
  sortProcesses,
  startupHelp,
  startupLabel,
  visibleServices,
} from "./lists";
import type { Tool } from "./model";
import { RegistryPanel } from "./registry-panel";
import { controlService, endProcesses, listProcesses, listServices, setServiceStartup } from "./tools-api";

// The tools' panels, loaded only when the tools page opens, so the
// website's one bundle doesn't carry them.

// The most rows a list draws; a search narrows the rest.
const MAX_ROWS = 2000;

type Problem = { message: string; code?: string };

const problemOf = (error: unknown, fallback: string): Problem => ({
  message: errorText(error, fallback),
  code: error instanceof RequestError ? error.code : undefined,
});

export default function ToolPanel({ agent, tool, canManage }: { agent: Agent; tool: Tool; canManage: boolean }) {
  if (tool === "registry") return <RegistryPanel agent={agent} canManage={canManage} />;
  return tool === "processes"
    ? <ProcessesPanel agent={agent} canManage={canManage} />
    : <ServicesPanel agent={agent} canManage={canManage} />;
}

function SortHeader<K extends string>({ label, column, sort, onSort, className }: {
  label: string;
  column: K;
  sort: Sort<K>;
  onSort: (key: K) => void;
  className?: string;
}) {
  const sorted = sort.key === column;
  return (
    <th scope="col" className={className} aria-sort={sorted ? (sort.descending ? "descending" : "ascending") : "none"}>
      <button type="button" className="sort-button" onClick={() => onSort(column)}>
        {label}
        {sorted && (sort.descending ? <ArrowDown size={13} aria-hidden="true" /> : <ArrowUp size={13} aria-hidden="true" />)}
      </button>
    </th>
  );
}

function ProblemBanner({ problem }: { problem: Problem }) {
  const help = errorHelp(problem.code);
  return (
    <div className="error-banner" role="alert">
      <CircleAlert size={16} aria-hidden="true" />
      <span>{problem.message}{help && problem.message !== help ? ` ${help}` : ""}</span>
    </div>
  );
}

// Whether the page is shown: a hidden tab asks the device for nothing.
function usePageVisible() {
  const [visible, setVisible] = useState(() => typeof document === "undefined" || document.visibilityState === "visible");
  useEffect(() => {
    const changed = () => setVisible(document.visibilityState === "visible");
    document.addEventListener("visibilitychange", changed);
    return () => document.removeEventListener("visibilitychange", changed);
  }, []);
  return visible;
}

type Confirming = { processes: ProcessInfo[]; tree: boolean };

function ProcessesPanel({ agent, canManage }: { agent: Agent; canManage: boolean }) {
  const { authorizedFetch } = useWorkspace();
  const pageVisible = usePageVisible();
  const [live, setLive] = useState(true);
  const [list, setList] = useState<ProcessList | null>(null);
  const [problem, setProblem] = useState<Problem | null>(null);
  const [reload, setReload] = useState(0);
  const [query, setQuery] = useState("");
  const [sort, setSort] = useState(DEFAULT_PROCESS_SORT);
  const [tree, setTree] = useState(false);
  const [collapsed, setCollapsed] = useState<ReadonlySet<number>>(new Set());
  const [chosen, setChosen] = useState<ReadonlySet<number>>(new Set());
  const [confirming, setConfirming] = useState<Confirming | null>(null);
  const [notice, setNotice] = useState<{ text: string; error?: boolean } | null>(null);
  const opener = useRef<HTMLButtonElement>(null);
  const polling = live && pageVisible && !confirming;

  const read = useCallback(async (signal: AbortSignal) => {
    try {
      const next = await listProcesses(authorizedFetch, agent.id, signal);
      if (signal.aborted) return;
      setList(next);
      setProblem(null);
    } catch (error) {
      if (signal.aborted || error instanceof AuthenticationRequired) return;
      setProblem(problemOf(error, "The device’s processes couldn’t be listed."));
    }
  }, [authorizedFetch, agent.id]);

  // Reads the list now, then every few seconds while the page is shown and
  // live; a hidden or paused page asks the device for nothing, except once
  // for Try again or after processes end.
  const readFor = useRef(0);
  useEffect(() => {
    const asked = reload !== readFor.current;
    readFor.current = reload;
    if (!polling && !asked) return;
    let timer: number | undefined;
    const controller = new AbortController();
    const loop = async () => {
      await read(controller.signal);
      if (polling && !controller.signal.aborted) timer = window.setTimeout(loop, PROCESS_REFRESH_MS);
    };
    void loop();
    return () => {
      controller.abort();
      window.clearTimeout(timer);
    };
  }, [polling, reload, read]);

  const processes = useMemo(() => list?.processes ?? [], [list]);
  const byPid = useMemo(() => new Map(processes.map((process) => [process.pid, process])), [processes]);
  const rows = useMemo(
    () => tree
      ? processTree(processes, sort, query, collapsed)
      : sortProcesses(processes.filter((process) => processMatches(process, query)), sort).map((process) => ({ process, depth: 0, children: 0 })),
    [processes, sort, query, tree, collapsed],
  );
  const chosenProcesses = [...chosen].map((pid) => byPid.get(pid)).filter((process): process is ProcessInfo => !!process);

  const ask = (processes: ProcessInfo[], tree: boolean) => {
    setNotice(null);
    setConfirming({ processes, tree });
  };
  const toggleCollapsed = (pid: number) => setCollapsed((current) => {
    const next = new Set(current);
    if (!next.delete(pid)) next.add(pid);
    return next;
  });
  const toggleChosen = (pid: number, on: boolean) => setChosen((current) => {
    const next = new Set(current);
    if (on) next.add(pid);
    else next.delete(pid);
    return next;
  });
  const ended = (outcome: ProcessesEnded) => {
    setChosen(new Set());
    const failed = outcome.failed.length;
    setNotice({
      text: failed
        ? `Ended ${outcome.ended.length} of ${outcome.ended.length + failed}. ${outcome.failed.map((failure) => failure.error.message).slice(0, 3).join(" ")}`
        : outcome.ended.length === 1 ? `Ended ${outcome.ended[0].name}.` : `Ended ${outcome.ended.length} processes.`,
      error: failed > 0,
    });
    setReload((count) => count + 1);
  };

  if (!list) {
    return problem
      ? <><ProblemBanner problem={problem} /><button type="button" className="secondary-button" onClick={() => setReload((count) => count + 1)}><RefreshCw size={15} /> Try again</button></>
      : <div className="empty-state compact"><LoaderCircle size={22} className="spin" aria-hidden="true" /><strong>Listing processes…</strong></div>;
  }
  const status = !live ? "Paused" : !pageVisible ? "Paused while this tab is hidden" : `Live, every ${PROCESS_REFRESH_MS / 1000} seconds`;
  return (
    <section className="agent-panel tools-panel" aria-label="Processes">
      <div className="table-toolbar">
        <label className="search-field">
          <Search size={16} aria-hidden="true" />
          <input type="search" aria-label="Search processes" value={query} onChange={(event) => setQuery(event.target.value)} placeholder="Name, PID, user or command line" />
        </label>
        <div className="segmented" role="group" aria-label="Show processes">
          <button type="button" aria-pressed={!tree} onClick={() => setTree(false)}>List</button>
          <button type="button" aria-pressed={tree} onClick={() => setTree(true)}><ListTree size={14} aria-hidden="true" />Tree</button>
        </div>
        <span className="tools-status" aria-live="polite">
          <i className={`led ${live && pageVisible ? "online" : "offline"}`} aria-hidden="true" />
          {status} · {processes.length.toLocaleString()} processes · read {formatDateTime(list.read_at)}
        </span>
        <div className="selection-actions">
          <button type="button" className="secondary-button" onClick={() => setLive((on) => !on)}>
            {live ? <><Pause size={15} /> Pause</> : <><Play size={15} /> Resume</>}
          </button>
          {canManage && chosenProcesses.length > 0 && (
            <button ref={opener} type="button" className="danger-button" disabled={chosenProcesses.length > MAX_ENDED_PROCESSES} onClick={() => ask(chosenProcesses, false)}>
              <XCircle size={15} /> End {chosenProcesses.length} selected
            </button>
          )}
        </div>
      </div>
      {problem && <ProblemBanner problem={problem} />}
      {notice && <p className={notice.error ? "installer-error tools-notice" : "field-help tools-notice"} role="status">{notice.text}</p>}
      {list.truncated && <p className="field-help tools-notice">The device runs more processes than one list carries; it lists the first {processes.length.toLocaleString()}.</p>}
      <div className="table-scroll">
        <table className="data-table process-table" aria-label="Processes">
          <thead>
            <tr>
              {canManage && <th scope="col" className="select-cell"><span className="sr-only">Select</span></th>}
              <SortHeader label="Name" column="name" sort={sort} onSort={(key: ProcessSortKey) => setSort(nextSort(sort, key, NUMERIC_PROCESS_KEYS))} />
              <SortHeader label="PID" column="pid" sort={sort} onSort={(key: ProcessSortKey) => setSort(nextSort(sort, key, NUMERIC_PROCESS_KEYS))} className="numeric" />
              <SortHeader label="User" column="user" sort={sort} onSort={(key: ProcessSortKey) => setSort(nextSort(sort, key, NUMERIC_PROCESS_KEYS))} />
              <SortHeader label="CPU" column="cpu" sort={sort} onSort={(key: ProcessSortKey) => setSort(nextSort(sort, key, NUMERIC_PROCESS_KEYS))} className="numeric" />
              <SortHeader label="Memory" column="memory" sort={sort} onSort={(key: ProcessSortKey) => setSort(nextSort(sort, key, NUMERIC_PROCESS_KEYS))} className="numeric" />
              <SortHeader label="Started" column="started" sort={sort} onSort={(key: ProcessSortKey) => setSort(nextSort(sort, key, NUMERIC_PROCESS_KEYS))} />
              {agent.platform === "windows" && <th scope="col" className="numeric">Session</th>}
              {canManage && <th scope="col"><span className="sr-only">Actions</span></th>}
            </tr>
          </thead>
          <tbody>
            {rows.slice(0, MAX_ROWS).map(({ process, depth, children }) => (
              <tr key={process.pid} className={chosen.has(process.pid) ? "selected-row" : undefined}>
                {canManage && <td className="select-cell"><input type="checkbox" aria-label={`Select ${process.name} (${process.pid})`} checked={chosen.has(process.pid)} onChange={(event) => toggleChosen(process.pid, event.target.checked)} /></td>}
                <td className={`process-name tree-depth-${Math.min(depth, 12)}`}>
                  <strong>
                    {tree && children > 0 && !query.trim() ? (
                      <button type="button" className="tree-toggle" aria-expanded={!collapsed.has(process.pid)} aria-label={`${collapsed.has(process.pid) ? "Show" : "Hide"} what ${process.name} started`} onClick={() => toggleCollapsed(process.pid)}>
                        {collapsed.has(process.pid) ? <ChevronRight size={14} /> : <ChevronDown size={14} />}
                      </button>
                    ) : tree ? <span className="tree-toggle" aria-hidden="true" /> : null}
                    {process.name}
                  </strong>
                  {process.command_line && <span className="mono-detail" title={process.command_line}>{process.command_line}{process.command_line_truncated ? "…" : ""}</span>}
                </td>
                <td className="numeric">{process.pid}</td>
                <td>{process.user ?? "—"}</td>
                <td className="numeric">{formatCpu(process.cpu_percent)}</td>
                <td className="numeric">{formatBytes(process.memory_bytes)}</td>
                <td>{process.started_at ? formatDateTime(process.started_at) : "—"}</td>
                {agent.platform === "windows" && <td className="numeric">{process.session ?? "—"}</td>}
                {canManage && (
                  <td className="row-actions">
                    <button type="button" className="link-button" onClick={() => ask([process], false)}>End</button>
                    <button type="button" className="link-button" onClick={() => ask(processTreeOf(processes, process.pid), true)}>End tree</button>
                  </td>
                )}
              </tr>
            ))}
          </tbody>
        </table>
        {!rows.length && <p className="table-message field-help">No processes match.</p>}
        {rows.length > MAX_ROWS && <p className="table-message field-help">Showing {MAX_ROWS.toLocaleString()} of {rows.length.toLocaleString()}; search to find the rest.</p>}
      </div>
      {confirming && (
        <EndDialog
          agent={agent}
          confirming={confirming}
          returnFocus={opener}
          onClose={() => setConfirming(null)}
          onEnded={(outcome) => {
            setConfirming(null);
            ended(outcome);
          }}
        />
      )}
    </section>
  );
}

function ActionDialog({ id, title, busy, onClose, returnFocus, children }: {
  id: string;
  title: string;
  busy: boolean;
  onClose: () => void;
  returnFocus?: RefObject<HTMLElement | null>;
  children: ReactNode;
}) {
  return (
    <ModalDialog className="settings-modal file-dialog" labelledBy={id} onClose={busy ? () => {} : onClose} returnFocus={returnFocus}>
      <button type="button" className="modal-close" onClick={onClose} aria-label="Close" disabled={busy}><X size={19} /></button>
      <h2 id={id}>{title}</h2>
      {children}
    </ModalDialog>
  );
}

// Lists every process that will end, and ends exactly those.
function EndDialog({ agent, confirming, returnFocus, onClose, onEnded }: {
  agent: Agent;
  confirming: Confirming;
  returnFocus: RefObject<HTMLElement | null>;
  onClose: () => void;
  onEnded: (outcome: ProcessesEnded) => void;
}) {
  const { authorizedFetch } = useWorkspace();
  const id = useId();
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const { processes, tree } = confirming;
  const tooMany = processes.length > MAX_ENDED_PROCESSES;
  const submit = async (event: FormEvent) => {
    event.preventDefault();
    setBusy(true);
    setError(null);
    try {
      onEnded(await endProcesses(authorizedFetch, agent.id, processes, tree));
    } catch (failure) {
      if (!(failure instanceof AuthenticationRequired)) setError(errorText(failure, "The processes couldn’t be ended."));
      setBusy(false);
    }
  };
  const title = processes.length === 1 ? `End ${processes[0].name}?` : tree ? `End ${processes.at(-1)?.name} and ${processes.length - 1} it started?` : `End ${processes.length} processes?`;
  return (
    <ActionDialog id={`${id}-title`} title={title} busy={busy} onClose={onClose} returnFocus={returnFocus}>
      <p>{processes.length === 1 ? "This process ends" : "These processes end"}, without saving their work. {agent.platform === "windows" ? "Windows stops each at once." : "Each is asked to stop, and killed if it hasn’t after 5 seconds."}</p>
      <ul className="file-delete-list tools-end-list">
        {processes.map((process) => <li key={process.pid}><strong>{process.name}</strong> · PID {process.pid}{process.user ? ` · ${process.user}` : ""}</li>)}
      </ul>
      {tooMany && <p className="installer-error">At most {MAX_ENDED_PROCESSES} processes end at once.</p>}
      <form onSubmit={submit}>
        {error && <p role="alert" className="installer-error">{error}</p>}
        <div className="connection-reason-actions">
          <button type="button" className="secondary-button" onClick={onClose} disabled={busy}>Cancel</button>
          <button className="danger-button" disabled={busy || tooMany}>
            {busy ? <LoaderCircle size={16} className="spin" /> : <XCircle size={16} />} End {processes.length === 1 ? "process" : `${processes.length} processes`}
          </button>
        </div>
      </form>
    </ActionDialog>
  );
}

type ServiceDialog = { kind: "action"; service: ServiceInfo; action: ServiceAction } | { kind: "startup"; service: ServiceInfo };

function ServicesPanel({ agent, canManage }: { agent: Agent; canManage: boolean }) {
  const { authorizedFetch } = useWorkspace();
  const [list, setList] = useState<ServiceList | null>(null);
  const [problem, setProblem] = useState<Problem | null>(null);
  const [reload, setReload] = useState(0);
  // The reload the latest answer, or failure, was for.
  const [answered, setAnswered] = useState(-1);
  const loading = answered !== reload;
  const [query, setQuery] = useState("");
  const [filter, setFilter] = useState<ServiceFilter>("all");
  const [sort, setSort] = useState(DEFAULT_SERVICE_SORT);
  const [dialog, setDialog] = useState<ServiceDialog | null>(null);
  const [notice, setNotice] = useState<string | null>(null);
  const opener = useRef<HTMLElement | null>(null);

  useEffect(() => {
    const controller = new AbortController();
    listServices(authorizedFetch, agent.id, controller.signal).then(
      (next) => {
        setList(next);
        setProblem(null);
        setAnswered(reload);
      },
      (error: unknown) => {
        if (controller.signal.aborted || error instanceof AuthenticationRequired) return;
        setProblem(problemOf(error, "The device’s services couldn’t be listed."));
        setAnswered(reload);
      },
    );
    return () => controller.abort();
  }, [authorizedFetch, agent.id, reload]);

  const services = useMemo(() => list?.services ?? [], [list]);
  const shown = useMemo(() => visibleServices(services, filter, query, sort), [services, filter, query, sort]);
  const running = services.filter((service) => service.state === "running").length;
  const changed = useCallback((service: ServiceInfo, text: string) => {
    setList((current) => current && { ...current, services: current.services.map((each) => (each.name === service.name ? service : each)) });
    setNotice(text);
    setDialog(null);
  }, []);
  const onSort = (key: ServiceSortKey) => setSort(nextSort(sort, key, []));

  if (!list) {
    return problem
      ? <><ProblemBanner problem={problem} /><button type="button" className="secondary-button" onClick={() => setReload((count) => count + 1)}><RefreshCw size={15} /> Try again</button></>
      : <div className="empty-state compact"><LoaderCircle size={22} className="spin" aria-hidden="true" /><strong>Listing services…</strong></div>;
  }
  return (
    <section className="agent-panel tools-panel" aria-label="Services">
      <div className="table-toolbar">
        <label className="search-field">
          <Search size={16} aria-hidden="true" />
          <input type="search" aria-label="Search services" value={query} onChange={(event) => setQuery(event.target.value)} placeholder="Name, description or account" />
        </label>
        <div className="outcome-chips" role="group" aria-label="Show services">
          <button type="button" className="outcome-chip" aria-pressed={filter === "all"} onClick={() => setFilter("all")}>All<em>{services.length}</em></button>
          <button type="button" className="outcome-chip" aria-pressed={filter === "running"} onClick={() => setFilter("running")}>Running<em>{running}</em></button>
          <button type="button" className="outcome-chip" aria-pressed={filter === "stopped"} onClick={() => setFilter("stopped")}>Not running<em>{services.length - running}</em></button>
        </div>
        <span className="tools-status">Read {formatDateTime(list.read_at)}</span>
        <button type="button" className="secondary-button" disabled={loading} onClick={() => setReload((count) => count + 1)}>
          {loading ? <LoaderCircle size={15} className="spin" /> : <RefreshCw size={15} />} Refresh
        </button>
      </div>
      {problem && <ProblemBanner problem={problem} />}
      {notice && <p className="field-help tools-notice" role="status">{notice}</p>}
      {agent.platform === "macos" && <p className="field-help tools-notice">These are launchd’s system domain jobs. Each user’s own launch agents aren’t listed.</p>}
      <div className="table-scroll">
        <table className="data-table service-table" aria-label="Services">
          <thead>
            <tr>
              <SortHeader label="Service" column="name" sort={sort} onSort={onSort} />
              <SortHeader label="State" column="state" sort={sort} onSort={onSort} />
              <SortHeader label="Startup" column="startup" sort={sort} onSort={onSort} />
              <SortHeader label="Account" column="account" sort={sort} onSort={onSort} />
              <th scope="col" className="numeric">PID</th>
              {canManage && <th scope="col"><span className="sr-only">Actions</span></th>}
            </tr>
          </thead>
          <tbody>
            {shown.slice(0, MAX_ROWS).map((service) => (
              <tr key={service.name}>
                <td>
                  <strong>{service.display_name}</strong>
                  {service.display_name !== service.name && <span className="mono-detail">{service.name}</span>}
                  {service.description && <span title={service.description} className="service-description">{service.description}</span>}
                  {service.protected && <span className="badge badge-muted" title="The MeshRMM Agent's own service">MeshRMM Agent</span>}
                </td>
                <td title={service.state_detail}><span className={`service-state service-${service.state}`}>{STATE_LABELS[service.state]}</span></td>
                <td title={service.startup_detail}>{service.startup ? startupLabel(service.startup, agent.platform) : "—"}</td>
                <td>{service.account ?? "—"}</td>
                <td className="numeric">{service.pid ?? "—"}</td>
                {canManage && (
                  <td className="row-actions">
                    {serviceActions(service).map((action) => (
                      <button
                        key={action}
                        type="button"
                        className="link-button"
                        onClick={(event) => {
                          opener.current = event.currentTarget;
                          setNotice(null);
                          setDialog({ kind: "action", service, action });
                        }}
                      >
                        {ACTION_LABELS[action]}
                      </button>
                    ))}
                    {(service.startup_choices?.length ?? 0) > 0 && (
                      <button
                        type="button"
                        className="link-button"
                        onClick={(event) => {
                          opener.current = event.currentTarget;
                          setNotice(null);
                          setDialog({ kind: "startup", service });
                        }}
                      >
                        Startup…
                      </button>
                    )}
                  </td>
                )}
              </tr>
            ))}
          </tbody>
        </table>
        {!shown.length && <p className="table-message field-help">No services match.</p>}
        {shown.length > MAX_ROWS && <p className="table-message field-help">Showing {MAX_ROWS.toLocaleString()} of {shown.length.toLocaleString()}; search to find the rest.</p>}
      </div>
      {dialog?.kind === "action" && (
        <ServiceActionDialog agent={agent} service={dialog.service} action={dialog.action} returnFocus={opener} onClose={() => setDialog(null)} onDone={changed} />
      )}
      {dialog?.kind === "startup" && (
        <StartupDialog agent={agent} service={dialog.service} returnFocus={opener} onClose={() => setDialog(null)} onDone={changed} />
      )}
    </section>
  );
}

const ACTION_DONE: Record<ServiceAction, string> = { start: "Started", stop: "Stopped", restart: "Restarted" };

function ServiceActionDialog({ agent, service, action, returnFocus, onClose, onDone }: {
  agent: Agent;
  service: ServiceInfo;
  action: ServiceAction;
  returnFocus: RefObject<HTMLElement | null>;
  onClose: () => void;
  onDone: (service: ServiceInfo, notice: string) => void;
}) {
  const { authorizedFetch } = useWorkspace();
  const id = useId();
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const submit = async (event: FormEvent) => {
    event.preventDefault();
    setBusy(true);
    setError(null);
    try {
      const updated = await controlService(authorizedFetch, agent.id, service.name, action);
      onDone(updated, `${ACTION_DONE[action]} ${updated.display_name}; it is ${STATE_LABELS[updated.state].toLowerCase()}.`);
    } catch (failure) {
      if (!(failure instanceof AuthenticationRequired)) setError(errorText(failure, `${service.display_name} couldn’t be changed.`));
      setBusy(false);
    }
  };
  const Icon = action === "start" ? Play : action === "stop" ? Square : RotateCw;
  return (
    <ActionDialog id={`${id}-title`} title={`${ACTION_LABELS[action]} ${service.display_name}?`} busy={busy} onClose={onClose} returnFocus={returnFocus}>
      <p>
        {action === "start" ? "The service starts now." : action === "stop" ? "The service stops now, and whatever uses it loses it until it starts again." : "The service stops and starts again; whatever uses it loses it meanwhile."}
        {" "}It can take up to two minutes.
      </p>
      <form onSubmit={submit}>
        {error && <p role="alert" className="installer-error">{error}</p>}
        <div className="connection-reason-actions">
          <button type="button" className="secondary-button" onClick={onClose} disabled={busy}>Cancel</button>
          <button className={action === "start" ? "primary-button" : "danger-button"} disabled={busy}>
            {busy ? <LoaderCircle size={16} className="spin" /> : <Icon size={16} />} {ACTION_LABELS[action]}
          </button>
        </div>
      </form>
    </ActionDialog>
  );
}

function StartupDialog({ agent, service, returnFocus, onClose, onDone }: {
  agent: Agent;
  service: ServiceInfo;
  returnFocus: RefObject<HTMLElement | null>;
  onClose: () => void;
  onDone: (service: ServiceInfo, notice: string) => void;
}) {
  const { authorizedFetch } = useWorkspace();
  const id = useId();
  const choices = service.startup_choices ?? [];
  const [chosen, setChosen] = useState<ServiceStartup>(service.startup && choices.includes(service.startup) ? service.startup : choices[0]);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const platform: DevicePlatform = agent.platform;
  const submit = async (event: FormEvent) => {
    event.preventDefault();
    setBusy(true);
    setError(null);
    try {
      const updated = await setServiceStartup(authorizedFetch, agent.id, service.name, chosen);
      onDone(updated, `${updated.display_name} now starts: ${updated.startup ? startupLabel(updated.startup, platform) : "unknown"}.`);
    } catch (failure) {
      if (!(failure instanceof AuthenticationRequired)) setError(errorText(failure, "How the service starts couldn’t be changed."));
      setBusy(false);
    }
  };
  return (
    <ActionDialog id={`${id}-title`} title={`How ${service.display_name} starts`} busy={busy} onClose={onClose} returnFocus={returnFocus}>
      <form onSubmit={submit}>
        <fieldset className="choice-list" disabled={busy}>
          <legend className="sr-only">Startup type</legend>
          {choices.map((choice) => (
            <label key={choice}>
              <input type="radio" name={`${id}-startup`} checked={chosen === choice} onChange={() => setChosen(choice)} />
              <span><strong>{startupLabel(choice, platform)}</strong>{startupHelp(choice, platform)}</span>
            </label>
          ))}
        </fieldset>
        <p className="field-help">This changes how it starts from now on; it doesn’t start or stop it.</p>
        {error && <p role="alert" className="installer-error">{error}</p>}
        <div className="connection-reason-actions">
          <button type="button" className="secondary-button" onClick={onClose} disabled={busy}>Cancel</button>
          <button className="primary-button" disabled={busy || chosen === service.startup}>
            {busy ? <LoaderCircle size={16} className="spin" /> : <Cog size={16} />} Save
          </button>
        </div>
      </form>
    </ActionDialog>
  );
}
