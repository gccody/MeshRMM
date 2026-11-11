// Keeps the live device inventory socket connected: applies the snapshot and
// revision-ordered deltas, and reconnects with backoff. It never discards the
// inventory it already has; the hook shows it as stale.
import { type AlertEvent, parseAlertEvent } from "../alerts/model.ts";
import { type JobEvent, parseJobEvent } from "../automation/model.ts";
import { type MetricsReading, parseMetricsEvent } from "../metrics/model.ts";
import { type UpdateEvent, parseUpdateEvent } from "../updates/model.ts";
import type { SendWatch } from "./metrics-watch.ts";
import { parseAgentEvent } from "./model.ts";
import type { AgentDelta, AgentEvent, AgentSnapshot, Inventory } from "./types";

const FIRST_RECONNECT_DELAY_MS = 1_000;
const MAX_RECONNECT_DELAY_MS = 30_000;
// While the server stays unreachable, refused sockets ask for a session
// check no more often than this; each check is two HTTP requests per tab.
const SESSION_CHECK_INTERVAL_MS = 60_000;
const MAX_PENDING_EVENTS = 1_000;
// Browsers don't show the page the server's pings, so a live socket that
// has heard nothing for this long may be dead, as after the computer slept.
// A socket may also rightly stay quiet, as one watching no device's
// readings in a fleet where nothing changes, so on the tab's next wake such
// a socket is asked for a `pong` and replaced only if none comes within
// PROBE_ANSWER_MS. Silence is measured against the clock when each message
// is handled, not with a timer, so a background tab's throttled timers
// can't make a healthy socket look dead; handlers that run late only make
// it look fresher.
export const SILENT_SOCKET_MS = 30_000;
// How long a quiet socket has to answer a ping on wake, while the tab is
// shown and its timers run on time.
export const PROBE_ANSWER_MS = 5_000;
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

// Where the stream puts the inventory: a snapshot replaces it, then each
// change after it is applied in revision order.
export type InventoryTarget = {
  replace: (inventory: Inventory) => void;
  apply: (change: AgentDelta) => void;
};

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
  inventory: InventoryTarget;
  // The watched devices' latest resource usage, which carries no revision.
  onMetrics?: (readings: MetricsReading[]) => void;
  // Watched devices whose screen image the server replaced.
  onThumbnails?: (ids: string[]) => void;
  // Which devices' readings the socket sends: told of each socket as it
  // opens, which watches nothing until sent the set, and as it goes.
  watch?: { attach: (send: SendWatch) => void; detach: () => void };
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
function eventSequencer({ revision, inventory, requestSnapshot, overflow }: {
  revision: { current: number };
  inventory: InventoryTarget;
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
    // The waiting changes that follow it, checked for a gap before the
    // inventory is touched.
    const changes: AgentDelta[] = [];
    let nextRevision = event.revision;
    for (const pending of pendingEvents.sort((left, right) => left.revision - right.revision)) {
      if (pending.revision <= nextRevision) continue;
      if (pending.revision !== nextRevision + 1) {
        pendingEvents = [];
        awaitingSnapshot = true;
        requestSnapshot();
        return false;
      }
      if (pending.type !== "unknown") changes.push(pending);
      nextRevision = pending.revision;
    }
    pendingEvents = [];
    awaitingSnapshot = false;
    synced = true;
    revision.current = nextRevision;
    inventory.replace({ agents: event.agents, companies: event.companies ?? [], tags: event.tags ?? [] });
    for (const change of changes) inventory.apply(change);
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
    if (event.type !== "unknown") inventory.apply(event);
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

// What the page sends the server.
export const REFRESH = JSON.stringify({ type: "refresh" });
export const PING = JSON.stringify({ type: "ping" });
export const watchMessage = (devices: readonly string[]) => JSON.stringify({ type: "watch", devices });

// How a socket ended: the server refused it, closed it for lost access, or
// dropped it for any other reason.
type SocketEnd = "refused" | "revoked" | "dropped";

// Handles one socket's events while it is the stream's current socket.
function watchSocket(socket: SocketLike, { revision, inventory, onMetrics, onThumbnails, onAlerts, onJobs, onUpdates, now, reportError, current, onOpen, onHeard, onLive, onUnusable, onClose }: {
  revision: { current: number };
  inventory: InventoryTarget;
  onMetrics?: (readings: MetricsReading[]) => void;
  onThumbnails?: (ids: string[]) => void;
  onAlerts?: (event: AlertEvent) => void;
  onJobs?: (event: JobEvent) => void;
  onUpdates?: (event: UpdateEvent) => void;
  now: () => number;
  reportError: (error: unknown) => void;
  current: () => boolean;
  onOpen: () => void;
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
    socket.send(REFRESH);
  };
  const sequencer = eventSequencer({
    revision,
    inventory,
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
    if (message.type === "thumbnails" && Array.isArray(message.device_ids)) {
      deliver(onThumbnails, message.device_ids.filter((id) => typeof id === "string"));
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
    onOpen();
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
  inventory,
  onMetrics,
  onThumbnails,
  watch,
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
  // The wait for a quiet socket's `pong`, while one is asked for.
  let probe: unknown;
  const cancelProbe = () => {
    if (probe === undefined) return;
    timers.clearTimeout(probe);
    probe = undefined;
  };
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
    cancelProbe();
    watch?.detach();
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
    inventory,
    onMetrics,
    onThumbnails,
    onAlerts,
    onJobs,
    onUpdates,
    now: timers.now,
    reportError,
    current: () => !stopped && socket === nextSocket,
    onOpen: () => watch?.attach((devices) => {
      if (socket === nextSocket && nextSocket.readyState === SOCKET_OPEN) nextSocket.send(watchMessage(devices));
    }),
    onHeard: () => {
      heardAt = timers.now();
      cancelProbe();
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
    // pressed. A live socket kept receiving changes in the background, and
    // one still connecting or awaiting its snapshot is about to deliver it,
    // so neither asks the server to send the whole fleet again. A live
    // socket that went silent is asked for a `pong`, and replaced if it
    // sends nothing in time, since a dead one would deliver no snapshot.
    // Without a socket it skips the backoff wait but keeps the step: only
    // going live restarts it.
    wake() {
      if (stopped) return;
      if (!socket) {
        connect();
        return;
      }
      if (!live || timers.now() - heardAt <= SILENT_SOCKET_MS || probe !== undefined) return;
      const quiet = socket;
      const replace = () => {
        drop(quiet, "website replaced a silent socket");
        connect();
      };
      if (quiet.readyState !== SOCKET_OPEN) {
        replace();
        return;
      }
      quiet.send(PING);
      probe = timers.setTimeout(() => {
        probe = undefined;
        if (!stopped && socket === quiet) replace();
      }, PROBE_ANSWER_MS);
    },
    setOnline(next: boolean) {
      connection.setOnline(next);
    },
    stop() {
      stopped = true;
      reconnect.cancel();
      cancelProbe();
      watch?.detach();
      socket?.close(1000, "website closed the inventory");
      socket = null;
    },
  };
}

// What the Refresh button does. A live stream is current: it has every
// change since its snapshot, and asks for a new snapshot itself when it
// misses one. So Refresh only has it check its socket, which replaces one
// that went silent, as a dead one would, and downloads nothing. Asking the
// server for a snapshot instead would read and send the whole fleet just to
// show what the page already holds. While the stream is down, Refresh skips
// the reconnect wait and loads the list over HTTP meanwhile.
export function refreshInventory(connection: InventoryConnection, { wake, load }: {
  wake: () => void;
  load: () => Promise<boolean>;
}) {
  wake();
  return connection === "live" ? Promise.resolve(true) : load();
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
