"use client";

import { useState } from "react";
import { AuthenticationRequired, type AuthorizedFetch, errorMessage } from "../../lib/http";
import type { Agent } from "../agents/types";
import { remoteViewerLink } from "./remote-link";

type Options = {
  authorizedFetch: AuthorizedFetch;
  reportError: (message: string | null) => void;
};

// Starts remote sessions through a one-time handoff to the native viewer, and
// closes a device's active session.
export function useRemoteHandoff({ authorizedFetch, reportError }: Options) {
  const [connectingId, setConnectingId] = useState<string | null>(null);
  const [connectingBackgroundId, setConnectingBackgroundId] = useState<string | null>(null);
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

  const connect = async (agent: Agent, startInBackground = false) => {
    setSessionNotice(null);
    if (!agent.connected) return;
    setConnectingId(agent.id);
    setConnectingBackgroundId(startInBackground ? agent.id : null);
    reportError(null);
    try {
      const response = await authorizedFetch("/v1/remote/handoffs", {
        method: "POST",
        headers: { "Content-Type": "application/json" },
        body: JSON.stringify({ device_id: agent.id, start_in_background: startInBackground }),
      });
      if (!response.ok) throw new Error(await errorMessage(response, "The remote session could not be started."));
      const handoff = (await response.json()) as { handoff_token: string; api_url: string; start_in_background?: boolean };
      if (startInBackground && handoff.start_in_background !== true) {
        throw new Error("Background launch requires an updated server. Try again after the server is updated.");
      }
      window.location.assign(remoteViewerLink(handoff.handoff_token, handoff.api_url, agent.id));
    } catch (requestError) {
      reportError(requestError instanceof Error ? requestError.message : "The remote session could not be started.");
    } finally {
      // Keep the spinner while the browser hands the link to the viewer.
      window.setTimeout(() => {
        setConnectingId(null);
        setConnectingBackgroundId(null);
      }, 1200);
    }
  };

  return { connectingId, connectingBackgroundId, closingId, sessionNotice, connect, closeSession };
}
