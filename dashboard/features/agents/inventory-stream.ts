// Keeps the live device inventory socket connected: applies the snapshot and
// revision-ordered deltas, and reconnects with backoff. It never discards the
// inventory it already has; the hook shows it as stale.
import { type MetricsReading, parseMetricsEvent } from "../metrics/model.ts";
import { applyAgentDelta, parseAgentEvent, sortAgents } from "./model.ts";
import type { Agent, AgentDelta, AgentEvent, AgentSnapshot } from "./types";

const FIRST_RECONNECT_DELAY_MS = 1_000;
const MAX_RECONNECT_DELAY_MS = 30_000;
const MAX_PENDING_EVENTS = 1_000;
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
};

const globalTimers: Timers = {
  setTimeout: (callback, ms) => setTimeout(callback, ms),
  clearTimeout: (timer) => clearTimeout(timer as ReturnType<typeof setTimeout>),
};

type Options = {
  openSocket: () => SocketLike;
  // Shared with the HTTP inventory load so neither applies older data.
  revision: { current: number };
  onAgents: (update: (current: Agent[]) => Agent[]) => void;
  // Devices' latest resource usage, which carries no revision.
  onMetrics?: (readings: MetricsReading[]) => void;
  onConnection: (connection: InventoryConnection) => void;
  // The server refused the socket or closed it for lost access. A browser
  // can't see why a handshake failed, so the caller checks the session.
  onRefused: () => void;
  online?: boolean;
  timers?: Timers;
};

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

// Waits before each reconnect, doubling the delay up to the maximum.
function reconnectBackoff(timers: Timers, stopped: () => boolean, connect: () => void) {
  let timer: unknown;
  let delay = FIRST_RECONNECT_DELAY_MS;
  return {
    cancel() {
      if (timer === undefined) return;
      timers.clearTimeout(timer);
      timer = undefined;
    },
    reset() {
      delay = FIRST_RECONNECT_DELAY_MS;
    },
    schedule() {
      if (stopped() || timer !== undefined) return;
      timer = timers.setTimeout(() => {
        timer = undefined;
        connect();
      }, delay);
      delay = Math.min(delay * 2, MAX_RECONNECT_DELAY_MS);
    },
  };
}

// Applies one socket's events in revision order. Deltas wait for its first
// snapshot, and a gap asks the server for a new one. Returns whether the
// event brought the inventory up to date.
function eventSequencer({ revision, onAgents, requestSnapshot, overflow }: {
  revision: { current: number };
  onAgents: (update: (current: Agent[]) => Agent[]) => void;
  requestSnapshot: () => void;
  overflow: () => void;
}) {
  let awaitingSnapshot = true;
  let pendingEvents: AgentDelta[] = [];

  const applySnapshot = (event: AgentSnapshot) => {
    if (event.revision < revision.current) {
      requestSnapshot();
      return false;
    }
    let nextAgents = sortAgents(event.agents);
    let nextRevision = event.revision;
    for (const pending of pendingEvents.sort((left, right) => left.revision - right.revision)) {
      if (pending.revision <= nextRevision) continue;
      if (pending.revision !== nextRevision + 1) {
        pendingEvents = [];
        awaitingSnapshot = true;
        requestSnapshot();
        return false;
      }
      nextAgents = applyAgentDelta(nextAgents, pending);
      nextRevision = pending.revision;
    }
    pendingEvents = [];
    awaitingSnapshot = false;
    revision.current = nextRevision;
    onAgents(() => nextAgents);
    return true;
  };

  const applyDelta = (event: AgentDelta) => {
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
    onAgents((agents) => applyAgentDelta(agents, event));
    return true;
  };

  return (event: AgentEvent | null) => {
    if (!event) {
      requestSnapshot();
      return false;
    }
    return event.type === "snapshot" ? applySnapshot(event) : applyDelta(event);
  };
}

// Handles one socket's events while it is the stream's current socket.
// `onClose` learns whether the server refused the socket or revoked access.
function watchSocket(socket: SocketLike, { revision, onAgents, onMetrics, current, onOpen, onLive, onClose }: {
  revision: { current: number };
  onAgents: (update: (current: Agent[]) => Agent[]) => void;
  onMetrics?: (readings: MetricsReading[]) => void;
  current: () => boolean;
  onOpen: () => void;
  onLive: () => void;
  onClose: (refused: boolean) => void;
}) {
  let opened = false;
  const requestSnapshot = () => {
    if (socket.readyState === SOCKET_OPEN) socket.send("refresh");
  };
  const sequence = eventSequencer({
    revision,
    onAgents,
    requestSnapshot,
    overflow: () => socket.close(1009, "too many pending device events"),
  });

  socket.addEventListener("open", () => {
    if (!current()) return;
    opened = true;
    onOpen();
  });
  socket.addEventListener("message", (message) => {
    const data = (message as MessageEvent).data;
    if (!current() || typeof data !== "string") return;
    try {
      const event: unknown = JSON.parse(data);
      const readings = parseMetricsEvent(event);
      if (readings) onMetrics?.(readings);
      else if (sequence(parseAgentEvent(event))) onLive();
    } catch {
      requestSnapshot();
    }
  });
  socket.addEventListener("error", () => socket.close());
  socket.addEventListener("close", (event) => {
    if (!current()) return;
    onClose(!opened || (event as CloseEvent).code === ACCESS_REVOKED_CLOSE_CODE);
  });
}

export function inventoryStream({
  openSocket,
  revision,
  onAgents,
  onMetrics,
  onConnection,
  onRefused,
  online = true,
  timers = globalTimers,
}: Options) {
  let stopped = false;
  let socket: SocketLike | null = null;
  const connection = connectionReporter(onConnection, online, () => stopped);
  const reconnect = reconnectBackoff(timers, () => stopped, () => connect());

  const listen = (nextSocket: SocketLike) => watchSocket(nextSocket, {
    revision,
    onAgents,
    onMetrics,
    current: () => !stopped && socket === nextSocket,
    onOpen: () => reconnect.reset(),
    onLive: () => connection.setState("live"),
    onClose: (refused) => {
      socket = null;
      connection.setState("reconnecting");
      if (refused) onRefused();
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
    // Resynchronizes now: asks an open socket for a fresh snapshot, or skips
    // the backoff wait and reconnects with the delay reset to 1 s.
    wake() {
      if (stopped) return;
      if (socket) {
        if (socket.readyState === SOCKET_OPEN) socket.send("refresh");
        return;
      }
      reconnect.cancel();
      reconnect.reset();
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
