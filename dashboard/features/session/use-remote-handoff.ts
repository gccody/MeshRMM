
import { useCallback, useEffect, useRef, useState } from "react";
import { AuthenticationRequired, type AuthorizedFetch, errorMessage } from "../../lib/http";
import type { Agent } from "../agents/types";
import { remoteViewerLink } from "./remote-link";
import { type ViewerLaunchOutcome, browserLaunchEnv, openExternalLink, watchViewerLaunch } from "./viewer-launch";

type Options = {
  authorizedFetch: AuthorizedFetch;
  reportError: (message: string | null) => void;
};

// The latest Connect: "opening" until the handoff is created and the page shows
// (or doesn't show) that the viewer took over.
export type ViewerLaunch = {
  attempt: number;
  agentId: string;
  agentName: string;
  background: boolean;
  // Why the technician connected, for the device user's approval prompt.
  reason: string;
  phase: "opening" | ViewerLaunchOutcome;
};

type Handoff = { handoff_token: string; api_url: string };

async function requestHandoff(authorizedFetch: AuthorizedFetch, agent: Agent, startInBackground: boolean, reason: string) {
  const response = await authorizedFetch("/v1/remote/handoffs", {
    method: "POST",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify({ device_id: agent.id, start_in_background: startInBackground, reason }),
  });
  if (!response.ok) throw new Error(await errorMessage(response, "The remote session could not be started."));
  return (await response.json()) as Handoff;
}

// Closes a device's active remote session and says how that went.
function useSessionClose({ authorizedFetch, reportError }: Options) {
  const [closingId, setClosingId] = useState<string | null>(null);
  const [sessionNotice, setSessionNotice] = useState<string | null>(null);

  const closeSession = async (agent: Agent) => {
    setClosingId(agent.id);
    reportError(null);
    setSessionNotice(null);
    try {
      const response = await authorizedFetch(`/v1/agents/${encodeURIComponent(agent.id)}/close-session`, { method: "POST" });
      if (!response.ok) throw new Error(await errorMessage(response, "The remote session could not be closed."));
      const result = (await response.json()) as { closed: boolean };
      setSessionNotice(result.closed
        ? `Closed the remote session for ${agent.name}. You can connect again.`
        : `${agent.name} has no active remote session. You can connect now.`);
    } catch (requestError) {
      if (!(requestError instanceof AuthenticationRequired)) {
        reportError(requestError instanceof Error ? requestError.message : "The remote session could not be closed.");
      }
    } finally {
      setClosingId(null);
    }
  };

  return { closingId, sessionNotice, setSessionNotice, closeSession };
}

// Starts remote sessions through a one-time handoff to the native viewer, and
// closes a device's active session.
export function useRemoteHandoff({ authorizedFetch, reportError }: Options) {
  const [launch, setLaunch] = useState<ViewerLaunch | null>(null);
  const { closingId, sessionNotice, setSessionNotice, closeSession } = useSessionClose({ authorizedFetch, reportError });
  // Each Connect supersedes the previous one, including its launch watcher.
  const attempts = useRef(0);
  const cancelWatch = useRef<(() => void) | null>(null);

  const stopWatching = () => {
    cancelWatch.current?.();
    cancelWatch.current = null;
  };

  useEffect(() => () => {
    attempts.current += 1;
    cancelWatch.current?.();
    cancelWatch.current = null;
  }, []);

  const connect = async (agent: Agent, startInBackground = false, reason = "") => {
    setSessionNotice(null);
    if (!agent.connected) return;
    stopWatching();
    const attempt = ++attempts.current;
    setLaunch({ attempt, agentId: agent.id, agentName: agent.name, background: startInBackground, reason, phase: "opening" });
    reportError(null);
    try {
      const handoff = await requestHandoff(authorizedFetch, agent, startInBackground, reason);
      if (attempt !== attempts.current) return;
      // Watch first: a viewer that is already running can take focus before
      // assign() returns.
      cancelWatch.current = watchViewerLaunch(browserLaunchEnv(), (phase) => {
        setLaunch((current) => (current?.attempt === attempt ? { ...current, phase } : current));
      });
      openExternalLink(remoteViewerLink(handoff.handoff_token, handoff.api_url, agent.id));
    } catch (requestError) {
      if (attempt !== attempts.current) return;
      setLaunch(null);
      if (!(requestError instanceof AuthenticationRequired)) {
        reportError(requestError instanceof Error ? requestError.message : "The remote session could not be started.");
      }
    }
  };

  // Hides the launch notice and stops watching for the viewer.
  const dismissLaunch = useCallback(() => {
    cancelWatch.current?.();
    cancelWatch.current = null;
    setLaunch(null);
  }, []);

  // Always a fresh handoff: tokens are single-use and expire after a minute.
  const retryLaunch = (agents: Agent[]) => {
    if (!launch) return;
    const agent = agents.find((candidate) => candidate.id === launch.agentId);
    if (!agent?.connected) {
      dismissLaunch();
      reportError(`${agent?.name ?? launch.agentName} is not online, so a remote session cannot start. Connect when it is back online.`);
      return;
    }
    void connect(agent, launch.background, launch.reason);
  };

  const opening = launch?.phase === "opening" ? launch : null;
  return {
    connectingId: opening?.agentId ?? null,
    connectingBackgroundId: opening?.background ? opening.agentId : null,
    closingId,
    sessionNotice,
    launch,
    connect,
    closeSession,
    dismissLaunch,
    retryLaunch,
  };
}
