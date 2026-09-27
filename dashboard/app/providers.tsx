"use client";

import { AuthProvider } from "../features/auth/auth-provider";
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
      <RuntimeConfigContext.Provider value={{ serverUrl, surface, hostname, tenantSlug, workosOrganizationId }}>
        {/* Carries the page baseline in globals.css. The WorkOS widgets bring
            their own Radix theme on the pages that use them. */}
        <div className="app-root">{children}</div>
      </RuntimeConfigContext.Provider>
    </AuthProvider>
  );
}
