"use client";

import { AuthProvider } from "../features/auth/auth-provider";
import { WorkOsWidgets } from "@workos-inc/widgets";
import { createContext, useContext } from "react";
export { LOGIN_ATTEMPT_KEY, AUTH_REFRESH_FAILED_EVENT } from "../features/auth/auth-provider";

type RuntimeConfig = {
  serverUrl: string;
  surface: "marketing" | "platform" | "tenant";
  hostname: string;
  tenantSlug?: string;
  workosOrganizationId?: string;
};

const RuntimeConfigContext = createContext<RuntimeConfig | null>(null);

export function useRuntimeConfig() {
  const config = useContext(RuntimeConfigContext);
  if (!config) throw new Error("Runtime configuration is unavailable.");
  return config;
}

export default function Providers({
  children,
  serverUrl,
  surface,
  hostname,
  tenantSlug,
  workosOrganizationId,
}: Readonly<{
  children: React.ReactNode;
  serverUrl: string;
  surface: RuntimeConfig["surface"];
  hostname: string;
  tenantSlug?: string;
  workosOrganizationId?: string;
}>) {
  return (
    <AuthProvider enabled={surface !== "marketing"}>
      <WorkOsWidgets theme={{ accentColor: "violet", radius: "medium", fontFamily: "var(--font-geist-sans)" }}>
        <RuntimeConfigContext.Provider value={{ serverUrl, surface, hostname, tenantSlug, workosOrganizationId }}>
          {children}
        </RuntimeConfigContext.Provider>
      </WorkOsWidgets>
    </AuthProvider>
  );
}
