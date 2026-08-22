"use client";

import { AuthKitProvider } from "@workos-inc/authkit-react";
import { WorkOsWidgets } from "@workos-inc/widgets";
import { createContext, useContext } from "react";
import { activityStorageKey } from "../features/session/idle-session";

type RuntimeConfig = {
  serverUrl: string;
};

export const LOGIN_ATTEMPT_KEY = "meshrmm:workos-login-attempt";
export const AUTH_REFRESH_FAILED_EVENT = "meshrmm:workos-refresh-failed";

type RedirectCallbackParams = {
  state?: Record<string, unknown> | null;
  organizationId?: string | null;
};

function handleRedirectCallback({ state, organizationId }: RedirectCallbackParams) {
  window.sessionStorage.removeItem(LOGIN_ATTEMPT_KEY);
  if (organizationId) {
    window.localStorage.setItem(activityStorageKey(organizationId), String(Date.now()));
  }

  const returnTo = state?.returnTo;
  if (typeof returnTo !== "string") return;

  try {
    const destination = new URL(returnTo, window.location.origin);
    if (destination.origin === window.location.origin) {
      window.history.replaceState({}, "", destination.href);
    }
  } catch {
    // Ignore malformed callback state and remain on the configured redirect URI.
  }
}

const RuntimeConfigContext = createContext<RuntimeConfig | null>(null);

function keepWorkOSSessionFresh() {
  return true;
}

export function useRuntimeConfig() {
  const config = useContext(RuntimeConfigContext);
  if (!config) throw new Error("Runtime configuration is unavailable.");
  return config;
}

export default function Providers({
  children,
  clientId,
  redirectUri,
  serverUrl,
}: Readonly<{
  children: React.ReactNode;
  clientId: string;
  redirectUri: string;
  serverUrl: string;
}>) {
  return (
    <AuthKitProvider
      clientId={clientId}
      redirectUri={redirectUri}
      onRedirectCallback={handleRedirectCallback}
      onBeforeAutoRefresh={keepWorkOSSessionFresh}
      onRefreshFailure={() => window.dispatchEvent(new Event(AUTH_REFRESH_FAILED_EVENT))}
    >
      <WorkOsWidgets theme={{ accentColor: "violet", radius: "medium", fontFamily: "var(--font-geist-sans)" }}>
        <RuntimeConfigContext.Provider value={{ serverUrl }}>
          {children}
        </RuntimeConfigContext.Provider>
      </WorkOsWidgets>
    </AuthKitProvider>
  );
}
