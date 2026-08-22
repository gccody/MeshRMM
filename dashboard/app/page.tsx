"use client";

import { LoginRequiredError, useAuth } from "@workos-inc/authkit-react";
import {
  AdminPortalDomainVerification,
  AdminPortalSsoConnection,
  OrganizationSwitcher,
  UsersManagement,
} from "@workos-inc/widgets";
import {
  Activity,
  Building2,
  ChevronDown,
  Clock3,
  KeyRound,
  LoaderCircle,
  LogOut,
  Menu,
  Monitor,
  Plus,
  RefreshCw,
  Search,
  Settings,
  ShieldCheck,
  Users,
  WifiOff,
  X,
} from "lucide-react";
import { FormEvent, useCallback, useEffect, useMemo, useState } from "react";
import { AUTH_REFRESH_FAILED_EVENT, useRuntimeConfig } from "./providers";
import { AgentOverview, type AgentStatusFilter } from "../features/agents/agent-overview";
import { useAgentInventory } from "../features/agents/use-agent-inventory";
import type { Agent } from "../features/agents/types";
import { EnrollmentModal, type AgentPlatform } from "../features/enrollment/enrollment-modal";
import { AuthenticationRequired, errorMessage, normalizeServer } from "../lib/http";
import {
  DEFAULT_IDLE_TIMEOUT_MINUTES,
  formatIdleTimeout,
} from "../features/session/idle-session";
import { useIdleSession } from "../features/session/use-idle-session";

type Company = { id: string; name: string; dashboard_idle_timeout_minutes: number };
type Account = {
  user_id: string;
  company: Company | null;
  role: string | null;
  roles: string[];
  permissions: string[];
};
type AgentInstallerBootstrap = {
  server: string;
  install_token: string;
  expires_at_unix_ms: number;
};
type View = "agents" | "team" | "sso";
type SessionPauseReason = "idle" | "expired";

const INSTALLER_ASSETS: Record<AgentPlatform, { label: string; binary: string; checksum: string }> = {
  "windows-x64": {
    label: "Windows 10/11 (x64)",
    binary: "/downloads/pulsermm-agent-windows-x64.exe",
    checksum: "/downloads/pulsermm-agent-windows-x64.exe.sha256",
  },
};
const ENROLLMENT_MAGIC = "PULSERMM-BOOTSTRAP-V1";
export default function Dashboard() {
  const { serverUrl } = useRuntimeConfig();
  const {
    isLoading: isAuthLoading,
    user,
    signIn,
    signOut,
    getAccessToken,
    organizationId,
    role,
    roles,
    switchToOrganization,
  } = useAuth();
  const [view, setView] = useState<View>("agents");
  const [account, setAccount] = useState<Account | null>(null);
  const [query, setQuery] = useState("");
  const [status, setStatus] = useState<AgentStatusFilter>("all");
  const [isAuthOpen, setIsAuthOpen] = useState(false);
  const [isOrganizationOpen, setIsOrganizationOpen] = useState(false);
  const [isAgentOpen, setIsAgentOpen] = useState(false);
  const [isSidebarOpen, setIsSidebarOpen] = useState(false);
  const [connectingId, setConnectingId] = useState<string | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [companyName, setCompanyName] = useState("");
  const [agentPlatform, setAgentPlatform] = useState<AgentPlatform>("windows-x64");
  const [isSaving, setIsSaving] = useState(false);
  const [isDownloadingInstaller, setIsDownloadingInstaller] = useState(false);
  const [installerDownloaded, setInstallerDownloaded] = useState(false);
  const [installerError, setInstallerError] = useState<string | null>(null);
  const [sessionPauseReason, setSessionPauseReason] = useState<SessionPauseReason | null>(null);
  const [isResumingSession, setIsResumingSession] = useState(false);
  const [idleTimeoutDraft, setIdleTimeoutDraft] = useState(DEFAULT_IDLE_TIMEOUT_MINUTES);
  const [isSavingSessionPolicy, setIsSavingSessionPolicy] = useState(false);

  const isAdmin = role === "admin" || roles?.includes("admin") || account?.role === "admin" || account?.roles.includes("admin");
  const companyId = account?.company?.id;
  const idleTimeoutMinutes = account?.company?.dashboard_idle_timeout_minutes ?? DEFAULT_IDLE_TIMEOUT_MINUTES;

  const lockSession = useCallback((reason: SessionPauseReason) => {
    setSessionPauseReason((current) => current ?? reason);
    setIsAuthOpen(false);
    setIsOrganizationOpen(false);
    setIsAgentOpen(false);
    setIsSidebarOpen(false);
  }, []);

  const authorizedFetch = useCallback(async (path: string, init: RequestInit = {}) => {
    let token: string;
    try {
      token = await getAccessToken();
    } catch (tokenError) {
      if (
        tokenError instanceof LoginRequiredError ||
        (tokenError instanceof Error && tokenError.message === "No access token available")
      ) {
        lockSession("expired");
        throw new AuthenticationRequired();
      }
      throw tokenError;
    }
    if (!token) throw new Error("Your WorkOS session has expired. Please sign in again.");
    const response = await fetch(`${normalizeServer(serverUrl)}${path}`, {
      ...init,
      headers: { ...init.headers, Authorization: `Bearer ${token}` },
    });
    if (response.status === 401) {
      lockSession("expired");
      throw new AuthenticationRequired();
    }
    return response;
  }, [getAccessToken, lockSession, serverUrl]);

  const { agents, isLive, isRefreshing, lastUpdated, loadAgents, reset: resetInventory } =
    useAgentInventory({
      enabled: Boolean(user && organizationId && !sessionPauseReason),
      companyId,
      authorizedFetch,
      reportError: setError,
    });

  const pauseIdleSession = useCallback(() => {
    resetInventory();
    lockSession("idle");
    void signOut({ navigate: false }).catch(() => {
      // The UI is already locked. A missing/expired WorkOS session needs no further cleanup.
    });
  }, [lockSession, resetInventory, signOut]);

  useIdleSession({
    enabled: Boolean(user && organizationId && !sessionPauseReason),
    organizationId,
    timeoutMinutes: idleTimeoutMinutes,
    onTimeout: pauseIdleSession,
  });

  useEffect(() => {
    const handleRefreshFailure = () => {
      resetInventory();
      lockSession("expired");
    };
    window.addEventListener(AUTH_REFRESH_FAILED_EVENT, handleRefreshFailure);
    return () => window.removeEventListener(AUTH_REFRESH_FAILED_EVENT, handleRefreshFailure);
  }, [lockSession, resetInventory]);

  const loadAccount = useCallback(async () => {
    if (!user || !organizationId) {
      setAccount(null);
      return null;
    }
    const response = await authorizedFetch("/v1/account");
    if (!response.ok) throw new Error(await errorMessage(response, "The company account could not be loaded."));
    const data = (await response.json()) as Account;
    setAccount(data);
    setIdleTimeoutDraft(data.company?.dashboard_idle_timeout_minutes ?? DEFAULT_IDLE_TIMEOUT_MINUTES);
    return data;
  }, [authorizedFetch, organizationId, user]);

  useEffect(() => {
    if (isAuthLoading || !user || !organizationId) return;
    let cancelled = false;
    const start = async () => {
      try {
        const loaded = await loadAccount();
        if (!cancelled && loaded?.company) await loadAgents();
      } catch (requestError) {
        if (!cancelled && !(requestError instanceof AuthenticationRequired)) {
          setError(requestError instanceof Error ? requestError.message : "The company account could not be loaded.");
        }
      }
    };
    void start();
    return () => { cancelled = true; };
  }, [isAuthLoading, loadAccount, loadAgents, organizationId, user]);

  const filteredAgents = useMemo(() => {
    const search = query.trim().toLowerCase();
    return agents.filter((agent) => {
      const matchesSearch = !search || `${agent.name} ${agent.id}`.toLowerCase().includes(search);
      const matchesStatus = status === "all" || (status === "online" ? agent.connected : !agent.connected);
      return matchesSearch && matchesStatus;
    });
  }, [agents, query, status]);

  const displayName = user ? [user.firstName, user.lastName].filter(Boolean).join(" ") || user.email : "Not signed in";
  const initials = user ? `${user.firstName?.[0] ?? user.email[0] ?? ""}${user.lastName?.[0] ?? ""}`.toUpperCase() : "--";
  const companyLabel = account?.company?.name ?? (organizationId ? "Provision company" : "Select company");

  const bootstrapCompany = async (event: FormEvent) => {
    event.preventDefault();
    setIsSaving(true);
    setError(null);
    try {
      const response = await authorizedFetch("/v1/company/bootstrap", {
        method: "POST",
        headers: { "Content-Type": "application/json" },
        body: JSON.stringify({ name: companyName }),
      });
      if (!response.ok) throw new Error(await errorMessage(response, "The company could not be provisioned."));
      const data = (await response.json()) as Account;
      setAccount(data);
      setIdleTimeoutDraft(data.company?.dashboard_idle_timeout_minutes ?? DEFAULT_IDLE_TIMEOUT_MINUTES);
      setCompanyName("");
      await loadAgents();
    } catch (requestError) {
      setError(requestError instanceof Error ? requestError.message : "The company could not be provisioned.");
    } finally {
      setIsSaving(false);
    }
  };

  const saveSessionPolicy = async (event: FormEvent) => {
    event.preventDefault();
    setIsSavingSessionPolicy(true);
    setError(null);
    try {
      const response = await authorizedFetch("/v1/company/settings", {
        method: "PUT",
        headers: { "Content-Type": "application/json" },
        body: JSON.stringify({ dashboard_idle_timeout_minutes: idleTimeoutDraft }),
      });
      if (!response.ok) {
        throw new Error(await errorMessage(response, "The session policy could not be saved."));
      }
      const data = (await response.json()) as Account;
      setAccount(data);
      setIdleTimeoutDraft(data.company?.dashboard_idle_timeout_minutes ?? DEFAULT_IDLE_TIMEOUT_MINUTES);
    } catch (requestError) {
      if (!(requestError instanceof AuthenticationRequired)) {
        setError(requestError instanceof Error ? requestError.message : "The session policy could not be saved.");
      }
    } finally {
      setIsSavingSessionPolicy(false);
    }
  };

  const resumeSession = async () => {
    setIsResumingSession(true);
    setError(null);
    try {
      await signIn({ organizationId: organizationId ?? undefined, state: { returnTo: "/" } });
    } catch (resumeError) {
      setError(resumeError instanceof Error ? resumeError.message : "The WorkOS session could not be resumed.");
      setIsResumingSession(false);
    }
  };

  const remoteInto = async (agent: Agent) => {
    if (!agent.connected) return;
    setConnectingId(agent.id);
    setError(null);
    try {
      const response = await authorizedFetch("/v1/remote/handoffs", {
        method: "POST",
        headers: { "Content-Type": "application/json" },
        body: JSON.stringify({ device_id: agent.id }),
      });
      if (!response.ok) throw new Error(await errorMessage(response, "A secure remote handoff could not be created."));
      const handoff = (await response.json()) as { handoff_token: string; api_url: string };
      window.location.assign(`pulsermm://connect?handoff=${encodeURIComponent(handoff.handoff_token)}&server=${encodeURIComponent(handoff.api_url)}`);
    } catch (requestError) {
      setError(requestError instanceof Error ? requestError.message : "A secure remote handoff could not be created.");
    } finally {
      window.setTimeout(() => setConnectingId(null), 1200);
    }
  };

  const downloadInstaller = async (event: FormEvent) => {
    event.preventDefault();
    setIsDownloadingInstaller(true);
    setInstallerError(null);
    try {
      const bootstrapResponse = await authorizedFetch("/v1/agent-installers", {
        method: "POST",
        headers: { "Content-Type": "application/json" },
        body: JSON.stringify({ platform: agentPlatform }),
      });
      if (!bootstrapResponse.ok) {
        throw new Error(await errorMessage(bootstrapResponse, "The Agent installer could not be authorized."));
      }
      const bootstrap = (await bootstrapResponse.json()) as AgentInstallerBootstrap;
      const asset = INSTALLER_ASSETS[agentPlatform];
      const [binaryResponse, checksumResponse] = await Promise.all([
        fetch(asset.binary, { cache: "no-store" }),
        fetch(asset.checksum, { cache: "no-store" }),
      ]);
      if (!binaryResponse.ok || !checksumResponse.ok) {
        throw new Error("The selected Agent installer has not been published yet.");
      }
      const binary = await binaryResponse.arrayBuffer();
      const expectedChecksum = (await checksumResponse.text()).trim().split(/\s+/)[0]?.toLowerCase();
      if (!expectedChecksum?.match(/^[a-f0-9]{64}$/)) {
        throw new Error("The published Agent installer checksum is invalid.");
      }
      const digest = await crypto.subtle.digest("SHA-256", binary);
      const actualChecksum = Array.from(new Uint8Array(digest), (byte) => byte.toString(16).padStart(2, "0")).join("");
      if (actualChecksum !== expectedChecksum) {
        throw new Error("The Agent installer failed its SHA-256 integrity check.");
      }

      const config = new TextEncoder().encode(JSON.stringify(bootstrap));
      const magic = new TextEncoder().encode(ENROLLMENT_MAGIC);
      const trailer = new Uint8Array(8 + magic.length);
      new DataView(trailer.buffer).setBigUint64(0, BigInt(config.length), true);
      trailer.set(magic, 8);
      const installer = new Blob([binary, config, trailer], { type: "application/vnd.microsoft.portable-executable" });
      const downloadUrl = URL.createObjectURL(installer);
      const link = document.createElement("a");
      link.href = downloadUrl;
      link.download = "PulseRMM-Agent-Setup-Windows-x64.exe";
      document.body.appendChild(link);
      link.click();
      link.remove();
      window.setTimeout(() => URL.revokeObjectURL(downloadUrl), 1_000);
      setInstallerDownloaded(true);
    } catch (downloadError) {
      setInstallerError(downloadError instanceof Error ? downloadError.message : "The Agent installer could not be created.");
    } finally {
      setIsDownloadingInstaller(false);
    }
  };

  const handleSignOut = () => {
    resetInventory();
    setAccount(null);
    setIsAuthOpen(false);
    signOut({ returnTo: "https://pulsermm.gccody.dev" });
  };

  const setActiveView = (next: View) => {
    setView(next);
    setIsSidebarOpen(false);
  };

  return (
    <div className="app-shell">
      <aside className={`sidebar ${isSidebarOpen ? "sidebar-open" : ""}`}>
        <div className="brand-row">
          <div className="brand-mark"><Activity size={19} strokeWidth={2.5} /></div>
          <span>Pulse<span>RMM</span></span>
          <button className="sidebar-close" onClick={() => setIsSidebarOpen(false)} aria-label="Close navigation"><X size={20} /></button>
        </div>

        <button className="workspace-switcher" onClick={() => setIsOrganizationOpen(true)} disabled={!user || Boolean(sessionPauseReason)}>
          <div className="workspace-avatar">{account?.company?.name.slice(0, 2).toUpperCase() ?? "CO"}</div>
          <div><strong>{companyLabel}</strong><span>{organizationId ? "WorkOS organization" : "Organization required"}</span></div>
          <ChevronDown size={16} />
        </button>

        <nav aria-label="Primary navigation">
          <p className="nav-label">Company</p>
          <button className={`nav-item ${view === "agents" ? "active" : ""}`} onClick={() => setActiveView("agents")} disabled={Boolean(sessionPauseReason)}><Monitor size={18} /><span>Agents</span>{isLive ? <em>{agents.length}</em> : null}</button>
          <button className={`nav-item ${view === "team" ? "active" : ""}`} onClick={() => setActiveView("team")} disabled={!organizationId || Boolean(sessionPauseReason)}><Users size={18} /><span>Users</span></button>
          <button className={`nav-item ${view === "sso" ? "active" : ""}`} onClick={() => setActiveView("sso")} disabled={!organizationId || Boolean(sessionPauseReason)}><KeyRound size={18} /><span>Single sign-on</span></button>
          <p className="nav-label nav-label-spaced">Account</p>
          <button className="nav-item" onClick={() => setIsAuthOpen(true)} disabled={Boolean(sessionPauseReason)}><Settings size={18} /><span>Profile & session</span></button>
        </nav>

        <button className="profile-row profile-button" onClick={() => setIsAuthOpen(true)} disabled={Boolean(sessionPauseReason)}>
          <div className="profile-avatar">{initials}</div>
          <div><strong>{displayName}</strong><span>{user?.email ?? "WorkOS authentication"}</span></div>
        </button>
      </aside>

      {isSidebarOpen && <button className="sidebar-scrim" onClick={() => setIsSidebarOpen(false)} aria-label="Close navigation" />}

      <main className="main-content">
        <header className="topbar">
          <button className="mobile-menu" onClick={() => setIsSidebarOpen(true)} aria-label="Open navigation" disabled={Boolean(sessionPauseReason)}><Menu size={21} /></button>
          {view === "agents" ? <label className="global-search"><Search size={18} /><input value={query} onChange={(event) => setQuery(event.target.value)} aria-label="Search Agents" placeholder="Search Agents by name or device ID..." /></label> : <div />}
          <div className="topbar-actions">
            <button className="connection-pill" onClick={() => setIsAuthOpen(true)} disabled={Boolean(sessionPauseReason)}>
              <span className={`pulse-dot ${isLive ? "live" : ""}`} />
              {sessionPauseReason ? "Session paused" : isAuthLoading ? "Checking session" : isLive ? "Service live" : user ? "Signed in" : "Sign in"}
            </button>
          </div>
        </header>

        <div className="page-wrap">
          {sessionPauseReason ? (
            <section className="signed-out-card session-paused-card">
              <div className="modal-icon"><Clock3 size={22} /></div>
              <p className="eyebrow">Session paused</p>
              <h1>{sessionPauseReason === "idle" ? "You’ve been signed out for inactivity" : "Your session needs to be renewed"}</h1>
              <p>{sessionPauseReason === "idle" ? `Your organization pauses inactive dashboards after ${formatIdleTimeout(idleTimeoutMinutes)}.` : "PulseRMM could not renew your WorkOS session. Your dashboard stayed in place and no organization data will be requested until you continue."}</p>
              <button className="primary-button" onClick={() => void resumeSession()} disabled={isResumingSession}>{isResumingSession ? <LoaderCircle size={16} className="spin" /> : <ShieldCheck size={16} />} Continue securely</button>
            </section>
          ) : !user && !isAuthLoading ? (
            <section className="signed-out-card">
              <div className="modal-icon"><ShieldCheck size={22} /></div>
              <p className="eyebrow">Secure company access</p>
              <h1>Sign in to PulseRMM</h1>
              <p>WorkOS authenticates each user and selects the company boundary before any Agent data is requested.</p>
              <button className="primary-button" onClick={() => void signIn({ state: { returnTo: "/" } })}><ShieldCheck size={16} /> Continue with WorkOS</button>
            </section>
          ) : user && !organizationId ? (
            <section className="signed-out-card organization-required">
              <div className="modal-icon"><Building2 size={22} /></div>
              <p className="eyebrow">Company required</p>
              <h1>Select your company</h1>
              <p>Your identity is valid, but PulseRMM only serves Agent data inside a WorkOS organization.</p>
              <div className="organization-widget"><OrganizationSwitcher authToken={getAccessToken} switchToOrganization={switchToOrganization} /></div>
            </section>
          ) : account && !account.company ? (
            <section className="signed-out-card organization-required">
              <div className="modal-icon"><Building2 size={22} /></div>
              <p className="eyebrow">First-time setup</p>
              <h1>Provision this company</h1>
              <p>This creates the tenant record in Cloudflare D1 for the selected WorkOS organization.</p>
              <form className="inline-provision-form" onSubmit={bootstrapCompany}>
                <input required maxLength={120} value={companyName} onChange={(event) => setCompanyName(event.target.value)} placeholder="Company name" aria-label="Company name" />
                <button className="primary-button" disabled={isSaving}>{isSaving ? <LoaderCircle size={16} className="spin" /> : <Building2 size={16} />} Provision company</button>
              </form>
            </section>
          ) : (
            <>
              <section className="page-heading">
                <div>
                  <p className="eyebrow">{account?.company?.name ?? "Company"}</p>
                  <h1>{view === "agents" ? "Agents" : view === "team" ? "Users" : "Single sign-on"}</h1>
                  <p>{view === "agents" ? "Live company inventory with secure, one-time remote handoffs." : view === "team" ? "Invite users and manage company roles through WorkOS." : "Configure company domains and an enterprise identity provider."}</p>
                </div>
                {view === "agents" && <div className="heading-actions">
                  <button className="secondary-button" onClick={() => void loadAgents()}><RefreshCw size={16} className={isRefreshing ? "spin" : ""} /> Refresh</button>
                  {isAdmin && <button className="primary-button" onClick={() => { setAgentPlatform("windows-x64"); setInstallerDownloaded(false); setInstallerError(null); setIsAgentOpen(true); }}><Plus size={16} /> Add Agent</button>}
                </div>}
              </section>

              {error && <div className="error-banner"><WifiOff size={17} /><span>{error}</span><button onClick={() => setError(null)} aria-label="Dismiss"><X size={16} /></button></div>}

              {view === "agents" ? (
                <AgentOverview
                    agents={agents}
                    filteredAgents={filteredAgents}
                    isLive={isLive}
                    lastUpdated={lastUpdated}
                    query={query}
                    status={status}
                    connectingId={connectingId}
                    onQueryChange={setQuery}
                    onStatusChange={setStatus}
                    onRemote={(agent) => void remoteInto(agent)}
                  />
                ) : view === "team" ? (
                <section className="management-panel"><UsersManagement authToken={getAccessToken} /></section>
              ) : (
                <div className="management-stack">
                  <section className="management-panel"><div className="management-heading"><h2>Company domains</h2><p>Verify a company domain before routing its users through SSO.</p></div><AdminPortalDomainVerification authToken={getAccessToken} /></section>
                  <section className="management-panel"><div className="management-heading"><h2>Identity provider</h2><p>Configure and maintain this company&apos;s SAML or OIDC connection.</p></div><AdminPortalSsoConnection authToken={getAccessToken} /></section>
                </div>
              )}
            </>
          )}
        </div>
      </main>

      {isAuthOpen && <div className="modal-backdrop" role="presentation" onMouseDown={(event) => event.target === event.currentTarget && setIsAuthOpen(false)}><section className="settings-modal" role="dialog" aria-modal="true" aria-labelledby="account-title"><button className="modal-close" onClick={() => setIsAuthOpen(false)} aria-label="Close"><X size={19} /></button><div className="modal-icon"><ShieldCheck size={22} /></div><p className="eyebrow">Authenticated</p><h2 id="account-title">WorkOS account</h2><p>Your session carries the selected organization and role used for every PulseRMM API request.</p>{user ? <><div className="account-summary"><div className="profile-avatar">{initials}</div><div><strong>{displayName}</strong><span>{user.email}</span></div></div>{account?.company && <div className="session-policy"><div className="session-policy-heading"><Clock3 size={16} /><div><strong>Organization idle timeout</strong><span>Currently {formatIdleTimeout(idleTimeoutMinutes)}</span></div></div>{isAdmin ? <form onSubmit={saveSessionPolicy}><label htmlFor="idle-timeout">Sign out inactive dashboards after<select id="idle-timeout" value={idleTimeoutDraft} onChange={(event) => setIdleTimeoutDraft(Number(event.target.value))}><option value={5}>5 minutes</option><option value={15}>15 minutes</option><option value={30}>30 minutes</option><option value={60}>1 hour</option><option value={120}>2 hours</option><option value={240}>4 hours</option><option value={480}>8 hours</option><option value={720}>12 hours</option><option value={1440}>24 hours</option></select></label><button className="primary-button" disabled={isSavingSessionPolicy || idleTimeoutDraft === idleTimeoutMinutes}>{isSavingSessionPolicy ? <LoaderCircle size={16} className="spin" /> : <Clock3 size={16} />} Save session policy</button></form> : <p>Only an organization administrator can change this policy.</p>}</div>}<button className="secondary-button modal-submit" onClick={handleSignOut}><LogOut size={16} /> Sign out</button></> : <button className="primary-button modal-submit" onClick={() => void signIn({ state: { returnTo: "/" } })}><ShieldCheck size={16} /> Continue with WorkOS</button>}</section></div>}

      {isOrganizationOpen && user && <div className="modal-backdrop" role="presentation" onMouseDown={(event) => event.target === event.currentTarget && setIsOrganizationOpen(false)}><section className="settings-modal organization-modal" role="dialog" aria-modal="true" aria-labelledby="organization-title"><button className="modal-close" onClick={() => setIsOrganizationOpen(false)} aria-label="Close"><X size={19} /></button><div className="modal-icon"><Building2 size={22} /></div><p className="eyebrow">Tenant boundary</p><h2 id="organization-title">Switch company</h2><p>Changing companies refreshes your WorkOS token before any other company inventory is requested.</p><div className="organization-widget"><OrganizationSwitcher authToken={getAccessToken} switchToOrganization={switchToOrganization} /></div></section></div>}

      {isAgentOpen && (
        <EnrollmentModal
          companyName={account?.company?.name}
          platform={agentPlatform}
          error={installerError}
          isDownloading={isDownloadingInstaller}
          downloaded={installerDownloaded}
          onClose={() => setIsAgentOpen(false)}
          onPlatformChange={setAgentPlatform}
          onSubmit={downloadInstaller}
        />
      )}
    </div>
  );
}
