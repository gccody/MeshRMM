"use client";

import { useCallback, useEffect, useRef, useState } from "react";
import { errorMessage } from "../../lib/http";
import { AuthenticationRequired } from "../../lib/http";
import { applyAgentDelta, parseAgentEvent, parseAgentList, sortAgents } from "./model";
import type { Agent, AgentDelta, AgentEventSubscription } from "./types";

const MAX_EVENT_RECONNECT_DELAY_MS = 30_000;

type AuthorizedFetch = (path: string, init?: RequestInit) => Promise<Response>;

type Options = {
  enabled: boolean;
  companyId?: string;
  authorizedFetch: AuthorizedFetch;
  reportError: (message: string | null) => void;
};

export function useAgentInventory({
  enabled,
  companyId,
  authorizedFetch,
  reportError,
}: Options) {
  const [agents, setAgents] = useState<Agent[]>([]);
  const [isLive, setIsLive] = useState(false);
  const [isRefreshing, setIsRefreshing] = useState(false);
  const [lastUpdated, setLastUpdated] = useState(() => new Date());
  const revision = useRef(-1);

  const reset = useCallback(() => {
    setAgents([]);
    setIsLive(false);
    revision.current = -1;
  }, []);

  const loadAgents = useCallback(
    async (silent = false) => {
      if (!enabled) return false;
      if (!silent) setIsRefreshing(true);
      reportError(null);
      try {
        const response = await authorizedFetch("/v1/agents");
        if (!response.ok) {
          throw new Error(
            await errorMessage(response, "The live agent service could not be reached."),
          );
        }
        const data = parseAgentList(await response.json());
        if (!data) throw new Error("The live agent service returned an invalid response.");
        if (data.revision < revision.current) return true;
        revision.current = data.revision;
        setAgents(sortAgents(data.agents));
        setIsLive(true);
        setLastUpdated(new Date());
        return true;
      } catch (requestError) {
        if (requestError instanceof AuthenticationRequired) return false;
        setIsLive(false);
        setAgents([]);
        reportError(
          requestError instanceof Error
            ? requestError.message
            : "The live agent service could not be reached.",
        );
        return false;
      } finally {
        setIsRefreshing(false);
      }
    },
    [authorizedFetch, enabled, reportError],
  );

  useEffect(() => {
    if (!enabled || !companyId) return;
    revision.current = -1;
    let disposed = false;
    let socket: WebSocket | null = null;
    let reconnectTimer: number | undefined;
    let reconnectDelay = 1_000;
    let awaitingSnapshot = true;
    let pendingEvents: AgentDelta[] = [];

    const scheduleReconnect = () => {
      if (disposed || reconnectTimer !== undefined) return;
      reconnectTimer = window.setTimeout(() => {
        reconnectTimer = undefined;
        void connect();
      }, reconnectDelay);
      reconnectDelay = Math.min(reconnectDelay * 2, MAX_EVENT_RECONNECT_DELAY_MS);
    };

    const connect = async () => {
      try {
        const response = await authorizedFetch("/v1/agents/events/subscriptions", {
          method: "POST",
        });
        if (!response.ok) {
          throw new Error(
            await errorMessage(response, "The live Agent event stream could not be opened."),
          );
        }
        const subscription = (await response.json()) as AgentEventSubscription;
        if (disposed) return;
        const websocketUrl = new URL(subscription.websocket_url);
        websocketUrl.searchParams.set("token", subscription.subscription_token);
        const nextSocket = new WebSocket(websocketUrl);
        socket = nextSocket;
        awaitingSnapshot = true;
        pendingEvents = [];

        const requestSnapshot = () => {
          if (nextSocket.readyState === WebSocket.OPEN) nextSocket.send("refresh");
        };

        nextSocket.addEventListener("open", () => {
          if (disposed || socket !== nextSocket) return;
          reconnectDelay = 1_000;
          setIsLive(true);
          reportError(null);
        });
        nextSocket.addEventListener("message", (message) => {
          if (disposed || socket !== nextSocket || typeof message.data !== "string") return;
          try {
            const event = parseAgentEvent(JSON.parse(message.data));
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
              for (const pending of pendingEvents.sort(
                (left, right) => left.revision - right.revision,
              )) {
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
              setAgents(nextAgents);
            } else {
              if (awaitingSnapshot) {
                pendingEvents.push(event);
                if (pendingEvents.length > 1_000) {
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
              setAgents((current) => applyAgentDelta(current, event));
            }
            setIsLive(true);
            setLastUpdated(new Date());
          } catch {
            requestSnapshot();
          }
        });
        nextSocket.addEventListener("error", () => nextSocket.close());
        nextSocket.addEventListener("close", () => {
          if (disposed || socket !== nextSocket) return;
          socket = null;
          setIsLive(false);
          scheduleReconnect();
        });
      } catch (requestError) {
        if (disposed || requestError instanceof AuthenticationRequired) return;
        setIsLive(false);
        reportError(
          requestError instanceof Error
            ? requestError.message
            : "The live Agent event stream could not be opened.",
        );
        scheduleReconnect();
      }
    };

    void connect();
    return () => {
      disposed = true;
      if (reconnectTimer !== undefined) window.clearTimeout(reconnectTimer);
      socket?.close(1000, "dashboard subscription ended");
    };
  }, [authorizedFetch, companyId, enabled, reportError]);

  return {
    agents: enabled ? agents : [],
    isLive: enabled && isLive,
    isRefreshing,
    lastUpdated,
    loadAgents,
    reset,
  };
}
