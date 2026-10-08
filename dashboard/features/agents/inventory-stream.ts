// Keeps the live device inventory socket connected: applies the snapshot and
// revision-ordered deltas, and reconnects with backoff. It never discards the
// inventory it already has; the hook shows it as stale.
import { applyAgentDelta, parseAgentEvent, sortAgents } from "./model.ts";
import type { Agent, AgentDelta } from "./types";

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
  onConnection: (connection: InventoryConnection) => void;
  // The server refused the socket or closed it for lost access. A browser
  // can't see why a handshake failed, so the caller checks the session.
  onRefused: () => void;
  online?: boolean;
  timers?: Timers;
};

export function inventoryStream({
  openSocket,
  revision,
  onAgents,
  onConnection,
  onRefused,
  online: initiallyOnline = true,
  timers = globalTimers,
}: Options) {
  let stopped = false;
  let socket: SocketLike | null = null;
  let timer: unknown;
  let delay = FIRST_RECONNECT_DELAY_MS;
  let state: Exclude<InventoryConnection, "offline"> = "connecting";
  let online = initiallyOnline;
  let reported: InventoryConnection | undefined;

  const report = () => {
    const next = online ? state : "offline";
    if (stopped || next === reported) return;
    reported = next;
    onConnection(next);
  };
  const setState = (next: typeof state) => {
    state = next;
    report();
  };

  const clearTimer = () => {
    if (timer === undefined) return;
    timers.clearTimeout(timer);
    timer = undefined;
  };

  const scheduleReconnect = () => {
    if (stopped || timer !== undefined) return;
    timer = timers.setTimeout(() => {
      timer = undefined;
      connect();
    }, delay);
    delay = Math.min(delay * 2, MAX_RECONNECT_DELAY_MS);
  };

  const listen = (nextSocket: SocketLike) => {
    let opened = false;
    let awaitingSnapshot = true;
    let pendingEvents: AgentDelta[] = [];
    const current = () => !stopped && socket === nextSocket;
    const requestSnapshot = () => {
      if (nextSocket.readyState === SOCKET_OPEN) nextSocket.send("refresh");
    };

    nextSocket.addEventListener("open", () => {
      if (!current()) return;
      opened = true;
      delay = FIRST_RECONNECT_DELAY_MS;
    });
    nextSocket.addEventListener("message", (message) => {
      const data = (message as MessageEvent).data;
      if (!current() || typeof data !== "string") return;
      try {
        const event = parseAgentEvent(JSON.parse(data));
        if (!event) {
          requestSnapshot();
          return;
        }
        if (event.type === "snapshot") {
          if (event.revision < revision.current) {
            requestSnapshot();
            return;
          }
          let nextAgents = sortAgents(event.agents);
          let nextRevision = event.revision;
          for (const pending of pendingEvents.sort((left, right) => left.revision - right.revision)) {
            if (pending.revision <= nextRevision) continue;
            if (pending.revision !== nextRevision + 1) {
              pendingEvents = [];
              awaitingSnapshot = true;
              requestSnapshot();
              return;
            }
            nextAgents = applyAgentDelta(nextAgents, pending);
            nextRevision = pending.revision;
          }
          pendingEvents = [];
          awaitingSnapshot = false;
          revision.current = nextRevision;
          onAgents(() => nextAgents);
        } else {
          if (awaitingSnapshot) {
            pendingEvents.push(event);
            if (pendingEvents.length > MAX_PENDING_EVENTS) {
              nextSocket.close(1009, "too many pending device events");
            }
            return;
          }
          if (event.revision <= revision.current) return;
          if (event.revision !== revision.current + 1) {
            awaitingSnapshot = true;
            pendingEvents = [event];
            requestSnapshot();
            return;
          }
          revision.current = event.revision;
          onAgents((agents) => applyAgentDelta(agents, event));
        }
        setState("live");
      } catch {
        requestSnapshot();
      }
    });
    nextSocket.addEventListener("error", () => nextSocket.close());
    nextSocket.addEventListener("close", (event) => {
      if (!current()) return;
      socket = null;
      setState("reconnecting");
      if (!opened || (event as CloseEvent).code === ACCESS_REVOKED_CLOSE_CODE) onRefused();
      scheduleReconnect();
    });
  };

  const connect = () => {
    if (stopped || socket) return;
    clearTimer();
    try {
      const nextSocket = openSocket();
      socket = nextSocket;
      listen(nextSocket);
    } catch {
      setState("reconnecting");
      scheduleReconnect();
    }
  };

  report();
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
      clearTimer();
      delay = FIRST_RECONNECT_DELAY_MS;
      connect();
    },
    setOnline(next: boolean) {
      online = next;
      report();
    },
    stop() {
      stopped = true;
      clearTimer();
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
