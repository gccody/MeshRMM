// Keeps the live Agent inventory stream connected: opens a subscription,
// applies the snapshot and revision-ordered deltas, and reconnects with backoff.
// It never discards the inventory it already has; the hook shows it as stale.
import { errorMessage } from "../../lib/http.ts";
import { applyAgentDelta, parseAgentEvent, sortAgents } from "./model.ts";
import type { Agent, AgentDelta, AgentEventSubscription } from "./types";

const FIRST_RECONNECT_DELAY_MS = 1_000;
const MAX_RECONNECT_DELAY_MS = 30_000;
const MAX_PENDING_EVENTS = 1_000;
// The server rotates authorization by closing the socket (4001) and the stream
// reconnects after 1 s, so a short gap must not mark the inventory stale.
export const STALE_GRACE_MS = 5_000;
const SOCKET_OPEN = 1;

// "unavailable": the server refused the subscription (403/404). Retrying
// automatically cannot help; only an explicit wake() tries again.
export type InventoryConnection = "connecting" | "live" | "reconnecting" | "offline" | "unavailable";
export type InventoryStatus = "loading" | "live" | "stale";

export type SocketLike = EventTarget & {
  readonly readyState: number;
  send(data: string): void;
  close(code?: number, reason?: string): void;
};

type Renewal = { accept(value: unknown): boolean; stop(): void };

type Timers = {
  setTimeout: (callback: () => void, ms: number) => unknown;
  clearTimeout: (timer: unknown) => void;
};

const globalTimers: Timers = {
  setTimeout: (callback, ms) => setTimeout(callback, ms),
  clearTimeout: (timer) => clearTimeout(timer as ReturnType<typeof setTimeout>),
};

type Options = {
  // Requests a subscription. Resolves null when the session needs to sign in
  // again; the dashboard locks itself, so the stream just stops trying.
  subscribe: () => Promise<Response | null>;
  openSocket: (url: string) => SocketLike;
  renewal: (disconnect: () => void) => Renewal;
  // Shared with the HTTP inventory load so neither applies older data.
  revision: { current: number };
  onAgents: (update: (current: Agent[]) => Agent[]) => void;
  onConnection: (connection: InventoryConnection) => void;
  onError: (message: string | null) => void;
  online?: boolean;
  timers?: Timers;
};

class SubscriptionError extends Error {
  readonly status: number;

  constructor(message: string, status: number) {
    super(message);
    this.status = status;
  }
}

export function inventoryStream({
  subscribe,
  openSocket,
  renewal: startRenewal,
  revision,
  onAgents,
  onConnection,
  onError,
  online: initiallyOnline = true,
  timers = globalTimers,
}: Options) {
  let stopped = false;
  let connecting = false;
  let socket: SocketLike | null = null;
  let stopRenewal: (() => void) | undefined;
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
      void connect();
    }, delay);
    delay = Math.min(delay * 2, MAX_RECONNECT_DELAY_MS);
  };

  const listen = (nextSocket: SocketLike) => {
    const renewal = startRenewal(() => nextSocket.close(4001, "fresh authorization required"));
    stopRenewal = renewal.stop;
    let awaitingSnapshot = true;
    let pendingEvents: AgentDelta[] = [];
    const current = () => !stopped && socket === nextSocket;
    const requestSnapshot = () => {
      if (nextSocket.readyState === SOCKET_OPEN) nextSocket.send("refresh");
    };

    nextSocket.addEventListener("open", () => {
      if (!current()) return;
      delay = FIRST_RECONNECT_DELAY_MS;
      onError(null);
    });
    nextSocket.addEventListener("message", (message) => {
      const data = (message as MessageEvent).data;
      if (!current() || typeof data !== "string") return;
      try {
        const value: unknown = JSON.parse(data);
        if (renewal.accept(value)) return;
        const event = parseAgentEvent(value);
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
              nextSocket.close(1009, "too many pending Agent events");
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
    nextSocket.addEventListener("close", () => {
      renewal.stop();
      if (!current()) return;
      socket = null;
      setState("reconnecting");
      scheduleReconnect();
    });
  };

  const connect = async () => {
    if (stopped || connecting || socket) return;
    connecting = true;
    clearTimer();
    try {
      const response = await subscribe();
      if (stopped || !response) return;
      if (!response.ok) {
        throw new SubscriptionError(
          await errorMessage(response, "The live Agent event stream could not be opened."),
          response.status,
        );
      }
      const subscription = (await response.json()) as AgentEventSubscription;
      if (stopped) return;
      const websocketUrl = new URL(subscription.websocket_url);
      websocketUrl.searchParams.set("token", subscription.subscription_token);
      websocketUrl.searchParams.set("protocol", "2");
      const nextSocket = openSocket(websocketUrl.toString());
      socket = nextSocket;
      listen(nextSocket);
    } catch (requestError) {
      if (stopped) return;
      onError(
        requestError instanceof Error
          ? requestError.message
          : "The live Agent event stream could not be opened.",
      );
      // A refused subscription (no access, company gone) will not recover by itself.
      if (requestError instanceof SubscriptionError && (requestError.status === 403 || requestError.status === 404)) {
        setState("unavailable");
        return;
      }
      setState("reconnecting");
      scheduleReconnect();
    } finally {
      connecting = false;
    }
  };

  report();
  void connect();

  return {
    // Resynchronizes now: asks an open stream for a fresh snapshot, or skips
    // the backoff wait and reconnects with the delay reset to 1 s.
    wake() {
      if (stopped || connecting) return;
      if (socket) {
        if (socket.readyState === SOCKET_OPEN) socket.send("refresh");
        return;
      }
      clearTimer();
      delay = FIRST_RECONNECT_DELAY_MS;
      void connect();
    },
    setOnline(next: boolean) {
      online = next;
      report();
    },
    stop() {
      stopped = true;
      stopRenewal?.();
      clearTimer();
      socket?.close(1000, "dashboard subscription ended");
      socket = null;
    },
  };
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
