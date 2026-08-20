"use client";

import { AuthKitProvider } from "@workos-inc/authkit-react";
import { WorkOsWidgets } from "@workos-inc/widgets";
import { createContext, useContext } from "react";

type RuntimeConfig = {
  serverUrl: string;
};

export const LOGIN_ATTEMPT_KEY = "pulsermm:workos-login-attempt";

type RedirectCallbackParams = {
  state?: Record<string, unknown> | null;
};

function handleRedirectCallback({ state }: RedirectCallbackParams) {
  window.sessionStorage.removeItem(LOGIN_ATTEMPT_KEY);

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
    >
      <WorkOsWidgets theme={{ accentColor: "violet", radius: "medium", fontFamily: "var(--font-geist-sans)" }}>
        <RuntimeConfigContext.Provider value={{ serverUrl }}>
          {children}
        </RuntimeConfigContext.Provider>
      </WorkOsWidgets>
    </AuthKitProvider>
  );
}
