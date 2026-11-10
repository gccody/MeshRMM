// Keeps the live device inventory socket connected: applies the snapshot and
// revision-ordered deltas, and reconnects with backoff. It never discards the
// inventory it already has; the hook shows it as stale.
import { type AlertEvent, parseAlertEvent } from "../alerts/model.ts";
import { type JobEvent, parseJobEvent } from "../automation/model.ts";
import { type MetricsReading, parseMetricsEvent } from "../metrics/model.ts";
import { type UpdateEvent, parseUpdateEvent } from "../updates/model.ts";
import { applyInventoryDelta, parseAgentEvent, sortAgents, sortCompanies, sortTags } from "./model.ts";
import type { AgentDelta, AgentEvent, AgentSnapshot, Inventory } from "./types";

const FIRST_RECONNECT_DELAY_MS = 1_000;
const MAX_RECONNECT_DELAY_MS = 30_000;
// While the server stays unreachable, refused sockets ask for a session
// check no more often than this; each check is two HTTP requests per tab.
const SESSION_CHECK_INTERVAL_MS = 60_000;
const MAX_PENDING_EVENTS = 1_000;
// Browsers don't show the page the server's pings, so a live socket that
// has heard nothing for this long may be dead, as after the computer slept.
// While any device is online the server sends resource readings every 5 s,
// so this is six missed readings; with none online it may rightly stay
// silent, and such a tab reconnects on its next wake: a full snapshot, but
// only for a fleet with nothing online. Measured against the clock when
// each message is handled, not with a timer, so a background tab's
// throttled timers can't make a healthy socket look dead; handlers that run
// late only make it look fresher.
export const SILENT_SOCKET_MS = 30_000;
// The server answers `refresh` before anything newer it sends, or closes the
// socket if it can't read the devices, so only messages it sent before
// reading the request arrive in between; the snapshot itself arrives whole,
// however long a large fleet takes to read and send. A message handled this
// long after asking, with no snapshot yet, means the request went unread,
// and the socket is replaced. A socket that went silent instead is replaced
// on the tab's next wake, like any silent one.
export const SNAPSHOT_ANSWER_MS = 30_000;
// A short gap, such as a server restart, must not mark the inventory stale.
export const STALE_GRACE_MS = 5_000;
const SOCKET_OPEN = 1;
// The server closes with this when the session ended or the user may no
// longer see devices.
export const ACCESS_REVOKED_CLOSE_CODE = 4001;

export type InventoryConnection = "connecting" | "live" | "reconnecting" | "offline";
export type InventoryStatus = "loading" | "live" | "stale";

export type SocketLike = EventTarget & {
  readonly readyState: number;
  send(data: string): void;
  close(code?: number, reason?: string): void;
};

type Timers = {
  setTimeout: (callback: () => void, ms: number) => unknown;
  clearTimeout: (timer: unknown) => void;
  now: () => number;
};

const globalTimers: Timers = {
  setTimeout: (callback, ms) => setTimeout(callback, ms),
  clearTimeout: (timer) => clearTimeout(timer as ReturnType<typeof setTimeout>),
  now: () => Date.now(),
};

type Options = {
  openSocket: () => SocketLike;
  // Shared with the HTTP inventory load so neither applies older data. It
  // compares only within one socket's life: -1 when a socket closes, and the
  // next socket's first snapshot applies whatever its revision.
  revision: { current: number };
  onInventory: (update: (current: Inventory) => Inventory) => void;
  // Devices' latest resource usage, which carries no revision.
  onMetrics?: (readings: MetricsReading[]) => void;
  // Alerts raised or changed, and the unresolved counts, for users who may
  // see them; these carry no revision either.
  onAlerts?: (event: AlertEvent) => void;
  // Jobs whose progress changed, for users who may see them.
  onJobs?: (event: JobEvent) => void;
  // Devices' updates and update runs, for users who may see them.
  onUpdates?: (event: UpdateEvent) => void;
  onConnection: (connection: InventoryConnection) => void;
  // The server refused the socket or closed it for lost access. A browser
  // can't see why a handshake failed, so the caller checks the session. Kept
  // to about once a minute while the server stays unreachable.
  onRefused: () => void;
  online?: boolean;
  timers?: Timers;
  // In [0, 1), for the reconnect jitter.
  random?: () => number;
  // Where a listener's exception goes; the browser logs it as uncaught.
  reportError?: (error: unknown) => void;
};

// The events the sequencer orders. Typed by AgentEvent, so a new one can't
// be missed here.
const INVENTORY_EVENTS: Record<AgentEvent["type"], true> = {
  snapshot: true,
  agent_upsert: true,
  agent_deleted: true,
  company_upsert: true,
  company_deleted: true,
  tag_upsert: true,
  tag_deleted: true,
};

// A revision-numbered change of a type this page doesn't know, from a newer
// server. It changes nothing the page shows, but it takes its revision.
type UnknownChange = { type: "unknown"; revision: number };
type Change = AgentDelta | UnknownChange;

const applyChange = (inventory: Inventory, change: Change) =>
  change.type === "unknown" ? inventory : applyInventoryDelta(inventory, change);

// Reports the connection state, or "offline" while the browser is, once per
// change.
function connectionReporter(
  onConnection: (connection: InventoryConnection) => void,
  initiallyOnline: boolean,
  stopped: () => boolean,
) {
  let state: Exclude<InventoryConnection, "offline"> = "connecting";
  let online = initiallyOnline;
  let reported: InventoryConnection | undefined;
  const report = () => {
    const next = online ? state : "offline";
    if (stopped() || next === reported) return;
    reported = next;
    onConnection(next);
  };
  return {
    report,
    setState(next: typeof state) {
      state = next;
      report();
    },
    setOnline(next: boolean) {
      online = next;
      report();
    },
  };
}

// Waits before each reconnect, doubling the step up to the maximum. Every
// tab loses its socket at once when the server restarts, so each wait is
// drawn between half the step and the whole of it: the tabs spread out
// instead of all asking for their snapshot together, and the floor keeps a
// tab from retrying straight after a failure as full jitter's near-zero
// draws would.
function reconnectBackoff(timers: Timers, random: () => number, stopped: () => boolean, connect: () => void) {
  let timer: unknown;
  let step = FIRST_RECONNECT_DELAY_MS;
  return {
    cancel() {
      if (timer === undefined) return;
      timers.clearTimeout(timer);
      timer = undefined;
    },
    reset() {
      step = FIRST_RECONNECT_DELAY_MS;
    },
    schedule() {
      if (stopped() || timer !== undefined) return;
      timer = timers.setTimeout(() => {
        timer = undefined;
        connect();
      }, step / 2 + (step / 2) * random());
      step = Math.min(step * 2, MAX_RECONNECT_DELAY_MS);
    },
  };
}

// Applies one socket's events in revision order. Deltas wait for its first
// snapshot, and a gap or an unreadable change asks the server for a new one
// and holds later changes until it arrives, so a run of them asks once.
// `apply` returns whether the event brought the inventory up to date.
function eventSequencer({ revision, onInventory, requestSnapshot, overflow }: {
  revision: { current: number };
  onInventory: (update: (current: Inventory) => Inventory) => void;
  requestSnapshot: () => void;
  overflow: () => void;
}) {
  let awaitingSnapshot = true;
  // Revisions restart with the server, so until this socket's first snapshot
  // applies, the revision the page holds may be another server's.
  let synced = false;
  let pendingEvents: Change[] = [];

  // An HTTP load overtook this socket's snapshot. The socket sends every
  // change after its snapshot, so carry on from the newer inventory instead
  // of asking again.
  const keepNewerInventory = () => {
    const pending = pendingEvents.sort((left, right) => left.revision - right.revision);
    pendingEvents = [];
    awaitingSnapshot = false;
    for (const event of pending) applyDelta(event);
    return !awaitingSnapshot;
  };

  const applySnapshot = (event: AgentSnapshot) => {
    if (synced && event.revision < revision.current) return keepNewerInventory();
    let next: Inventory = { agents: sortAgents(event.agents), companies: sortCompanies(event.companies ?? []), tags: sortTags(event.tags ?? []) };
    let nextRevision = event.revision;
    for (const pending of pendingEvents.sort((left, right) => left.revision - right.revision)) {
      if (pending.revision <= nextRevision) continue;
      if (pending.revision !== nextRevision + 1) {
        pendingEvents = [];
        awaitingSnapshot = true;
        requestSnapshot();
        return false;
      }
      next = applyChange(next, pending);
      nextRevision = pending.revision;
    }
    pendingEvents = [];
    awaitingSnapshot = false;
    synced = true;
    revision.current = nextRevision;
    onInventory(() => next);
    return true;
  };

  const applyDelta = (event: Change) => {
    if (awaitingSnapshot) {
      pendingEvents.push(event);
      if (pendingEvents.length > MAX_PENDING_EVENTS) overflow();
      return false;
    }
    if (event.revision <= revision.current) return false;
    if (event.revision !== revision.current + 1) {
      awaitingSnapshot = true;
      pendingEvents = [event];
      requestSnapshot();
      return false;
    }
    revision.current = event.revision;
    if (event.type !== "unknown") onInventory((inventory) => applyInventoryDelta(inventory, event));
    return true;
  };

  return {
    apply: (event: AgentSnapshot | Change) => event.type === "snapshot" ? applySnapshot(event) : applyDelta(event),
    // A change this page couldn't read, so the inventory lacks it. A
    // snapshot already on its way covers it, since the server sends only
    // newer changes after it.
    broken() {
      if (awaitingSnapshot) return;
      awaitingSnapshot = true;
      pendingEvents = [];
      requestSnapshot();
    },
  };
}

// How a socket ended: the server refused it, closed it for lost access, or
// dropped it for any other reason.
type SocketEnd = "refused" | "revoked" | "dropped";

// Handles one socket's events while it is the stream's current socket.
function watchSocket(socket: SocketLike, { revision, onInventory, onMetrics, onAlerts, onJobs, onUpdates, now, reportError, current, onHeard, onLive, onUnusable, onClose }: {
  revision: { current: number };
  onInventory: (update: (current: Inventory) => Inventory) => void;
  onMetrics?: (readings: MetricsReading[]) => void;
  onAlerts?: (event: AlertEvent) => void;
  onJobs?: (event: JobEvent) => void;
  onUpdates?: (event: UpdateEvent) => void;
  now: () => number;
  reportError: (error: unknown) => void;
  current: () => boolean;
  onHeard: () => void;
  onLive: () => void;
  // The page can't carry on from this socket's events.
  onUnusable: (reason: string) => void;
  onClose: (end: SocketEnd) => void;
}) {
  let opened = false;
  // When this page last sent `refresh`, until a snapshot arrives. Only one
  // is outstanding: the snapshot answering it covers every gap found
  // meanwhile, and each is a full read of the fleet.
  let askedAt: number | null = null;
  const requestSnapshot = () => {
    if (askedAt !== null || socket.readyState !== SOCKET_OPEN) return;
    askedAt = now();
    socket.send("refresh");
  };
  const sequencer = eventSequencer({
    revision,
    onInventory,
    requestSnapshot,
    overflow: () => socket.close(1009, "too many pending device events"),
  });

  // A page's bug must cost neither a snapshot nor the events after it.
  const deliver = <T>(listener: ((value: T) => void) | undefined, value: T) => {
    try {
      listener?.(value);
    } catch (error) {
      reportError(error);
    }
  };

  const sequence = (event: AgentSnapshot | Change) => {
    if (event.type === "snapshot") askedAt = null;
    let upToDate;
    try {
      upToDate = sequencer.apply(event);
    } catch (error) {
      // The inventory and the revision it is held at may no longer agree,
      // and the sequencer may be part way through. A new socket's snapshot
      // rebuilds both from nothing, and one that fails every time waits out
      // a growing backoff, where asking again would bring it straight back.
      reportError(error);
      onUnusable("website could not apply a device change");
      return;
    }
    if (upToDate) onLive();
  };

  const handle = (data: string) => {
    let event: unknown;
    try {
      event = JSON.parse(data);
    } catch {
      return;
    }
    if (!event || typeof event !== "object" || typeof (event as { type?: unknown }).type !== "string") return;
    const message = event as Record<string, unknown>;
    if (Object.hasOwn(INVENTORY_EVENTS, message.type as string)) {
      const parsed = parseAgentEvent(message);
      if (parsed) sequence(parsed);
      // Asking again would bring the same unreadable snapshot straight
      // back, so the socket is replaced after the reconnect wait instead.
      else if (message.type === "snapshot") onUnusable("website could not read the device snapshot");
      else sequencer.broken();
      return;
    }
    const readings = parseMetricsEvent(message);
    const alert = readings ? null : parseAlertEvent(message);
    const job = readings || alert ? null : parseJobEvent(message);
    const update = readings || alert || job ? null : parseUpdateEvent(message);
    if (readings) deliver(onMetrics, readings);
    else if (alert) deliver(onAlerts, alert);
    else if (job) deliver(onJobs, job);
    else if (update) deliver(onUpdates, update);
    // A change of a type a newer server added: skipping its revision keeps
    // the changes after it in order. Anything else unrecognised, like an
    // unreadable reading, alert, job or update, carries no revision, so
    // dropping it leaves the inventory whole.
    else if (Number.isSafeInteger(message.revision) && (message.revision as number) >= 0) {
      sequence({ type: "unknown", revision: message.revision as number });
    }
  };

  socket.addEventListener("open", () => {
    if (!current()) return;
    opened = true;
  });
  socket.addEventListener("message", (message) => {
    const data = (message as MessageEvent).data;
    if (!current()) return;
    onHeard();
    if (typeof data === "string") handle(data);
    if (current() && askedAt !== null && now() - askedAt > SNAPSHOT_ANSWER_MS) onUnusable("website's snapshot request went unanswered");
  });
  socket.addEventListener("error", () => socket.close());
  socket.addEventListener("close", (event) => {
    if (!current()) return;
    if (!opened) onClose("refused");
    else onClose((event as CloseEvent).code === ACCESS_REVOKED_CLOSE_CODE ? "revoked" : "dropped");
  });
}

export function inventoryStream({
  openSocket,
  revision,
  onInventory,
  onMetrics,
  onAlerts,
  onJobs,
  onUpdates,
  onConnection,
  onRefused,
  online = true,
  timers = globalTimers,
  random = Math.random,
  reportError = (error) => globalThis.reportError(error),
}: Options) {
  let stopped = false;
  let socket: SocketLike | null = null;
  // Whether the socket delivered its snapshot, and when it last sent anything.
  let live = false;
  let heardAt = 0;
  // When a refused socket last asked for a session check; null once the
  // stream is live, so the next outage's first refusal checks at once.
  let sessionCheckedAt: number | null = null;
  const connection = connectionReporter(onConnection, online, () => stopped);
  const reconnect = reconnectBackoff(timers, random, () => stopped, () => connect());

  // A revoked socket always checks: the server said access changed. A
  // refusal may only mean the server is down, which the last check covers.
  const checkSession = (end: Exclude<SocketEnd, "dropped">) => {
    const now = timers.now();
    if (end === "refused" && sessionCheckedAt !== null && now - sessionCheckedAt < SESSION_CHECK_INTERVAL_MS) return;
    sessionCheckedAt = now;
    onRefused();
  };

  const lose = () => {
    socket = null;
    live = false;
    // The server may have restarted its revisions; an HTTP load until the
    // next socket must not be measured against the lost one's.
    revision.current = -1;
    connection.setState("reconnecting");
  };

  const drop = (dropped: SocketLike, reason: string) => {
    lose();
    // It is no longer the current socket, so its close event, whenever a
    // dead connection sends one, is ignored.
    dropped.close(1000, reason);
  };

  const listen = (nextSocket: SocketLike) => watchSocket(nextSocket, {
    revision,
    onInventory,
    onMetrics,
    onAlerts,
    onJobs,
    onUpdates,
    now: timers.now,
    reportError,
    current: () => !stopped && socket === nextSocket,
    onHeard: () => {
      heardAt = timers.now();
    },
    // Only a socket that delivered its snapshot resets the backoff; one the
    // server accepts and then fails, before or while sending it, keeps
    // backing off.
    onLive: () => {
      live = true;
      reconnect.reset();
      sessionCheckedAt = null;
      connection.setState("live");
    },
    // After the reconnect wait, which only a snapshot that applies
    // restarts, so a snapshot that fails on every socket is sent at most
    // every 30 s.
    onUnusable: (reason) => {
      drop(nextSocket, reason);
      reconnect.schedule();
    },
    onClose: (end) => {
      lose();
      if (end !== "dropped") checkSession(end);
      reconnect.schedule();
    },
  });

  const connect = () => {
    if (stopped || socket) return;
    reconnect.cancel();
    try {
      const nextSocket = openSocket();
      socket = nextSocket;
      listen(nextSocket);
    } catch {
      connection.setState("reconnecting");
      reconnect.schedule();
    }
  };

  connection.report();
  connect();

  return {
    // Called when the tab is shown, the network returns or Refresh is
    // pressed while the stream is down. A live socket kept receiving changes
    // in the background, and one still connecting or awaiting its snapshot
    // is about to deliver it, so neither asks the server to send the whole
    // fleet again. A live socket that went silent is replaced, since a dead
    // one would deliver no snapshot. Without a socket it skips the backoff
    // wait but keeps the step: only going live restarts it.
    wake() {
      if (stopped) return;
      if (!socket) {
        connect();
        return;
      }
      if (!live || timers.now() - heardAt <= SILENT_SOCKET_MS) return;
      drop(socket, "website replaced a silent socket");
      connect();
    },
    setOnline(next: boolean) {
      connection.setOnline(next);
    },
    stop() {
      stopped = true;
      reconnect.cancel();
      socket?.close(1000, "website closed the inventory");
      socket = null;
    },
  };
}

// The website's event socket, on this page's own origin.
export function eventsSocketUrl(location: Pick<Location, "protocol" | "host">) {
  return `${location.protocol === "https:" ? "wss:" : "ws:"}//${location.host}/v1/events`;
}

// Whether the inventory on screen is current. `since` is when the stream last
// stopped being live (or started connecting); a gap shorter than the grace
// period still counts as live.
export function inventoryStatus({ hasData, connection, since, now }: {
  hasData: boolean;
  connection: InventoryConnection;
  since: number | null;
  now: number;
}): InventoryStatus {
  if (!hasData) return "loading";
  if (connection === "live") return "live";
  if (since !== null && now - since < STALE_GRACE_MS) return "live";
  return "stale";
}
