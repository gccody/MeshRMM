"use client";

import { LoginRequiredError, useAuth } from "../auth/auth-provider";
import {
  AdminPortalDomainVerification,
  AdminPortalSsoConnection,
  UsersManagement,
} from "@workos-inc/widgets";
import {
  Network,
  Building2,
  ChevronRight,
  Clock3,
  KeyRound,
  LoaderCircle,
  LogOut,
  Menu,
  Monitor,
  Plus,
  RefreshCw,
  Settings,
  ShieldCheck,
  Users,
  WifiOff,
  X,
} from "lucide-react";
import { useRouter } from "next/navigation";
import { FormEvent, useCallback, useEffect, useMemo, useRef, useState } from "react";
import { AUTH_REFRESH_FAILED_EVENT, useRuntimeConfig } from "../../app/providers";
import { AgentOverview, type AgentStatusFilter } from "../agents/agent-overview";
import { useAgentInventory } from "../agents/use-agent-inventory";
import type { Agent } from "../agents/types";
import { EnrollmentModal, type AgentPlatform } from "../enrollment/enrollment-modal";
import { AuthenticationRequired, errorMessage, normalizeServer } from "../../lib/http";
import {
  DEFAULT_IDLE_TIMEOUT_MINUTES,
  formatIdleTimeout,
} from "../session/idle-session";
import { remoteViewerLink } from "../session/remote-link";
import { AccountLoadError, accountLoader } from "./account-load";
import { useIdleSession } from "../session/use-idle-session";
import { MarketingPage } from "../marketing/marketing-page";
import { PlatformDashboard } from "../platform/platform-dashboard";

const DEFAULT_BLACKOUT_MESSAGE = "This machine is under maintenance by {user_name}.";

type Company = { prevent_idle_lock: boolean; allow_idle_override: boolean; display_border: boolean; blackout_message: string; id: string; name: string; slug: string | null; status: string; dashboard_idle_timeout_minutes: number };
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
type View = "agents" | "team" | "sso" | "settings";
const VIEW_PATHS: Record<View, string> = { agents: "/", team: "/users", sso: "/authentication", settings: "/settings" };
const SETTINGS_TABS = [
  { id: "dashboard-security", label: "Dashboard security" },
  { id: "remote-sessions", label: "Remote sessions" },
  { id: "blackout", label: "Blackout message" },
] as const;
type SettingsTab = (typeof SETTINGS_TABS)[number]["id"];
type SessionPauseReason = "idle" | "expired";

const INSTALLER_ASSETS: Record<AgentPlatform, { label: string; binary: string; checksum: string }> = {
  "windows-x64": {
    label: "Windows 10/11 (x64)",
    binary: "/downloads/meshrmm-agent-windows-x64.exe",
    checksum: "/downloads/meshrmm-agent-windows-x64.exe.sha256",
  },
};
const ENROLLMENT_MAGIC = "MESHRMM-BOOTSTRAP-V1";
export default function Dashboard({ view = "agents" }: { view?: View }) {
  const { surface } = useRuntimeConfig();
  if (surface === "marketing") return <MarketingPage />;
  if (surface === "platform") return <PlatformDashboard />;
  return <TenantDashboard view={view} />;
}

function TenantDashboard({ view }: { view: View }) {
  const router = useRouter();
  const { serverUrl, workosOrganizationId } = useRuntimeConfig();
  const {
    isLoading: isAuthLoading,
    user,
    signIn,
    signOut,
    getAccessToken,
    organizationId,
    role,
    roles,
  } = useAuth();
  const [account, setAccount] = useState<Account | null>(null);
  const [accountError, setAccountError] = useState<{ message: string; retrying: boolean } | null>(null);
  const accountLoad = useRef<{ retry: () => void } | null>(null);
  const [signOutError, setSignOutError] = useState<string | null>(null);
  const [settingsTab, setSettingsTab] = useState<SettingsTab>("dashboard-security");
  const [query, setQuery] = useState("");
  const [status, setStatus] = useState<AgentStatusFilter>("all");
  const [isAuthOpen, setIsAuthOpen] = useState(false);
  const [isAgentOpen, setIsAgentOpen] = useState(false);
  const [isSidebarOpen, setIsSidebarOpen] = useState(false);
  const [connectingId, setConnectingId] = useState<string | null>(null);
  const [connectingBackgroundId, setConnectingBackgroundId] = useState<string | null>(null);
  const [closingId, setClosingId] = useState<string | null>(null);
  const [sessionNotice, setSessionNotice] = useState<string | null>(null);
  const [deletingId, setDeletingId] = useState<string | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [agentPlatform, setAgentPlatform] = useState<AgentPlatform>("windows-x64");
  const [isDownloadingInstaller, setIsDownloadingInstaller] = useState(false);
  const [installerDownloaded, setInstallerDownloaded] = useState(false);
  const [installerError, setInstallerError] = useState<string | null>(null);
  const [sessionPauseReason, setSessionPauseReason] = useState<SessionPauseReason | null>(null);
  const [isResumingSession, setIsResumingSession] = useState(false);
  const [preventIdleDraft, setPreventIdleDraft] = useState(true);
  const [allowIdleOverrideDraft, setAllowIdleOverrideDraft] = useState(true);
  const [displayBorderDraft, setDisplayBorderDraft] = useState(true);
  const [blackoutMessageDraft, setBlackoutMessageDraft] = useState(DEFAULT_BLACKOUT_MESSAGE);
  const [idleTimeoutDraft, setIdleTimeoutDraft] = useState(DEFAULT_IDLE_TIMEOUT_MINUTES);
  const [settingsNotice, setSettingsNotice] = useState<string | null>(null);
  const [isSavingSessionPolicy, setIsSavingSessionPolicy] = useState(false);

  const hasTenantSession = Boolean(user && workosOrganizationId && organizationId === workosOrganizationId);
  const isAdmin = Boolean(
    role === "admin" ||
    role === "company_admin" ||
    roles?.some((candidate) => candidate === "admin" || candidate === "company_admin") ||
    account?.role === "admin" ||
    account?.role === "company_admin" ||
    account?.roles.some((candidate) => candidate === "admin" || candidate === "company_admin") ||
    account?.permissions.includes("company:settings:manage"),
  );
  const companyId = account?.company?.id;
  const accountPending = hasTenantSession && !account;
  const idleTimeoutMinutes = account?.company?.dashboard_idle_timeout_minutes ?? DEFAULT_IDLE_TIMEOUT_MINUTES;

  const lockSession = useCallback((reason: SessionPauseReason) => {
    setSessionPauseReason((current) => current ?? reason);
    setIsAuthOpen(false);
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
    if (!token) throw new Error("Your session has expired. Please sign in again.");
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
      enabled: Boolean(hasTenantSession && !sessionPauseReason),
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
    enabled: Boolean(hasTenantSession && account?.company && !sessionPauseReason),
    organizationId: workosOrganizationId,
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

  const applyAccount = useCallback((data: Account) => {
    setAccount(data);
    setIdleTimeoutDraft(data.company?.dashboard_idle_timeout_minutes ?? DEFAULT_IDLE_TIMEOUT_MINUTES);
    setBlackoutMessageDraft(data.company?.blackout_message ?? DEFAULT_BLACKOUT_MESSAGE);
    setDisplayBorderDraft(data.company?.display_border ?? true);
    setPreventIdleDraft(data.company?.prevent_idle_lock ?? true);
    setAllowIdleOverrideDraft(data.company?.allow_idle_override ?? true);
  }, []);

  const fetchAccount = useCallback(async () => {
    const response = await authorizedFetch("/v1/account");
    if (!response.ok) {
      throw new AccountLoadError(await errorMessage(response, "The company account could not be loaded."), response.status);
    }
    return (await response.json()) as Account;
  }, [authorizedFetch]);

  // The account carries the idle policy, so the workspace stays hidden until it
  // loads. The event subscription supplies the initial inventory.
  useEffect(() => {
    if (isAuthLoading || !hasTenantSession || sessionPauseReason) return;
    const loader = accountLoader({
      load: fetchAccount,
      onLoaded: (data) => {
        setAccountError(null);
        applyAccount(data);
      },
      onError: (requestError, retryInMs) => {
        if (requestError instanceof AuthenticationRequired) return;
        setAccountError({
          message: requestError instanceof Error ? requestError.message : "The company account could not be loaded.",
          retrying: retryInMs !== null,
        });
      },
    });
    accountLoad.current = loader;
    return () => {
      loader.stop();
      accountLoad.current = null;
    };
  }, [applyAccount, fetchAccount, hasTenantSession, isAuthLoading, sessionPauseReason]);

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
  const companyLabel = account?.company?.name ?? "Company workspace";

  const saveSessionPolicy = async (event: FormEvent) => {
    event.preventDefault();
    setIsSavingSessionPolicy(true);
    setSettingsNotice(null);
    setError(null);
    try {
      const response = await authorizedFetch("/v1/company/settings", {
        method: "PUT",
        headers: { "Content-Type": "application/json" },
        body: JSON.stringify({ dashboard_idle_timeout_minutes: idleTimeoutDraft, blackout_message: blackoutMessageDraft, display_border: displayBorderDraft, prevent_idle_lock: preventIdleDraft, allow_idle_override: allowIdleOverrideDraft }),
      });
      if (!response.ok) {
        throw new Error(await errorMessage(response, "The session policy could not be saved."));
      }
      applyAccount((await response.json()) as Account);
      setSettingsNotice("Company settings saved. Remote defaults apply to new sessions.");
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
      await signIn({ organizationId: workosOrganizationId, state: { returnTo: "/" } });
    } catch (resumeError) {
      setError(resumeError instanceof Error ? resumeError.message : "Your session could not be resumed.");
      setIsResumingSession(false);
    }
  };

  const closeAgentSession = async (agent: Agent) => {
    setClosingId(agent.id);
    setError(null);
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
        setError(requestError instanceof Error ? requestError.message : "The remote session could not be closed.");
      }
    } finally {
      setClosingId(null);
    }
  };

  const remoteInto = async (agent: Agent, startInBackground = false) => {
    setSessionNotice(null);
    if (!agent.connected) return;
    setConnectingId(agent.id);
    setConnectingBackgroundId(startInBackground ? agent.id : null);
    setError(null);
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
      setError(requestError instanceof Error ? requestError.message : "The remote session could not be started.");
    } finally {
      window.setTimeout(() => {
        setConnectingId(null);
        setConnectingBackgroundId(null);
      }, 1200);
    }
  };

  const deleteAgent = async (agent: Agent) => {
    const confirmed = window.confirm(
      `Delete ${agent.name}? The Agent will uninstall itself and remove its local files. If it is offline, cleanup will run the next time it connects.`,
    );
    if (!confirmed) return;
    setDeletingId(agent.id);
    setError(null);
    try {
      const response = await authorizedFetch(`/v1/agents/${encodeURIComponent(agent.id)}`, {
        method: "DELETE",
      });
      if (!response.ok) {
        throw new Error(await errorMessage(response, "The Agent could not be deleted."));
      }
      // The backend publishes agent_deleted to the existing subscription.
    } catch (requestError) {
      if (!(requestError instanceof AuthenticationRequired)) {
        setError(requestError instanceof Error ? requestError.message : "The Agent could not be deleted.");
      }
    } finally {
      setDeletingId(null);
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
      link.download = "MeshRMM-Agent-Setup-Windows-x64.exe";
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

  const handleSignOut = async () => {
    resetInventory();
    setAccount(null);
    setIsAuthOpen(false);
    setSignOutError(null);
    try {
      await signOut({ returnTo: "https://meshrmm.com" });
    } catch {
      // The browser session is already cleared, but the WorkOS session may remain.
      setSignOutError("Sign-out could not be completed. Sign in and sign out again, or close your browser, to end your session.");
    }
  };

  const setActiveView = (next: View) => {
    router.push(VIEW_PATHS[next]);
    setIsSidebarOpen(false);
  };

  return (
    <div className="app-shell">
      <aside className={`sidebar ${isSidebarOpen ? "sidebar-open" : ""}`}>
        <div className="brand-row">
          <div className="brand-mark"><Network size={19} strokeWidth={2.5} /></div>
          <span>Mesh<span>RMM</span></span>
          <button className="sidebar-close" onClick={() => setIsSidebarOpen(false)} aria-label="Close navigation"><X size={20} /></button>
        </div>

        <div className="workspace-switcher workspace-identity">
          <div className="workspace-avatar">{account?.company?.name.slice(0, 2).toUpperCase() ?? "CO"}</div>
          <div><strong>{companyLabel}</strong><span>Company workspace</span></div>
        </div>

        <nav aria-label="Primary navigation">
          <p className="nav-label">Company</p>
          <button className={`nav-item ${view === "agents" ? "active" : ""}`} aria-current={view === "agents" ? "page" : undefined} onClick={() => setActiveView("agents")} disabled={Boolean(sessionPauseReason)}><Monitor size={18} /><span>Devices</span>{isLive ? <em>{agents.length}</em> : null}</button>
          {isAdmin && <button className={`nav-item ${view === "team" ? "active" : ""}`} aria-current={view === "team" ? "page" : undefined} onClick={() => setActiveView("team")} disabled={!hasTenantSession || Boolean(sessionPauseReason)}><Users size={18} /><span>Users</span></button>}
          {isAdmin && <button className={`nav-item ${view === "sso" ? "active" : ""}`} aria-current={view === "sso" ? "page" : undefined} onClick={() => setActiveView("sso")} disabled={!hasTenantSession || Boolean(sessionPauseReason)}><KeyRound size={18} /><span>Authentication</span></button>}
          <button className={`nav-item ${view === "settings" ? "active" : ""}`} onClick={() => setActiveView("settings")} disabled={!hasTenantSession || Boolean(sessionPauseReason)} aria-current={view === "settings" ? "page" : undefined}><Settings size={18} /><span>Settings</span></button>
        </nav>

        <button className="profile-row profile-button" onClick={() => { setIsSidebarOpen(false); setIsAuthOpen(true); }} disabled={Boolean(sessionPauseReason)} aria-haspopup="dialog">
          <div className="profile-avatar">{initials}</div>
          <div className="profile-details"><strong>Your account</strong><span>{displayName}</span></div>
          <ChevronRight size={16} aria-hidden="true" />
        </button>
      </aside>

      {isSidebarOpen && <button className="sidebar-scrim" onClick={() => setIsSidebarOpen(false)} aria-label="Close navigation" />}

      <main className="main-content">
        <header className="topbar">
          <button className="mobile-menu" onClick={() => setIsSidebarOpen(true)} aria-label="Open navigation" aria-expanded={isSidebarOpen} disabled={Boolean(sessionPauseReason)}><Menu size={21} /></button>
          <div className="workspace-breadcrumb"><Building2 size={16} /><span>{companyLabel}</span></div>
          <div className="topbar-actions">
            <div className="connection-pill" role="status">
              <span className={`status-dot ${isLive ? "live" : ""}`} />
              {sessionPauseReason ? "Session paused" : isAuthLoading ? "Checking session" : isLive ? "Connected" : user ? "Signed in" : "Signed out"}
            </div>
          </div>
        </header>

        <div className="page-wrap">
          {sessionPauseReason ? (
            <section className="signed-out-card session-paused-card">
              <div className="modal-icon"><Clock3 size={22} /></div>
              <p className="eyebrow">Session paused</p>
              <h1>{sessionPauseReason === "idle" ? "You’ve been signed out for inactivity" : "Your session needs to be renewed"}</h1>
              <p>{sessionPauseReason === "idle" ? `Your organization pauses inactive dashboards after ${formatIdleTimeout(idleTimeoutMinutes)}.` : "Sign in again to continue managing your devices."}</p>
              <button className="primary-button" onClick={() => void resumeSession()} disabled={isResumingSession}>{isResumingSession ? <LoaderCircle size={16} className="spin" /> : <ShieldCheck size={16} />} Continue securely</button>
            </section>
          ) : !user && !isAuthLoading ? (
            <section className="signed-out-card">
              <div className="modal-icon"><ShieldCheck size={22} /></div>
              <p className="eyebrow">Secure company access</p>
              <h1>Sign in to MeshRMM</h1>
              <p>Sign in with your company account to manage devices and start remote sessions.</p>
              {signOutError && <p role="alert">{signOutError}</p>}
              <button className="primary-button" onClick={() => void signIn({ organizationId: workosOrganizationId, state: { returnTo: "/" } })}><ShieldCheck size={16} /> Sign in securely</button>
            </section>
          ) : user && !hasTenantSession ? (
            <section className="signed-out-card organization-required">
              <div className="modal-icon"><Building2 size={22} /></div>
              <p className="eyebrow">Company-specific access</p>
              <h1>Continue to this company</h1>
              <p>Sign in with an account that has access to this company’s workspace.</p>
              {signOutError && <p role="alert">{signOutError}</p>}
              <button className="primary-button" onClick={() => void signOut({ navigate: false })
                .then(() => signIn({ organizationId: workosOrganizationId, state: { returnTo: "/" } }))
                .catch(() => setSignOutError("Your session could not be switched. Please retry."))}><ShieldCheck size={16} /> Continue to company</button>
            </section>
          ) : account && !account.company ? (
            <section className="signed-out-card organization-required">
              <div className="modal-icon"><Building2 size={22} /></div>
              <p className="eyebrow">Workspace unavailable</p>
              <h1>This company is not ready</h1>
              <p>Contact your administrator to finish setting up this workspace.</p>
            </section>
          ) : (
            <>
              <section className="page-heading">
                <div>
                  <p className="eyebrow">{account?.company?.name ?? "Company"}</p>
                  <h1>{view === "agents" ? "Devices" : view === "team" ? "Users" : view === "settings" ? "Settings" : "Authentication"}</h1>
                  <p>{view === "agents" ? "Connect to your devices and keep your team working." : view === "team" ? "Invite your team and manage their access." : view === "settings" ? "Manage company security and remote session defaults." : "Manage company domains and single sign-on."}</p>
                </div>
                {view === "agents" && !accountPending && <div className="heading-actions">
                  <button className="secondary-button" onClick={() => void loadAgents()}><RefreshCw size={16} className={isRefreshing ? "spin" : ""} /> Refresh</button>
                  {isAdmin && <button className="primary-button" onClick={() => { setAgentPlatform("windows-x64"); setInstallerDownloaded(false); setInstallerError(null); setIsAgentOpen(true); }}><Plus size={16} /> Add device</button>}
                </div>}
              </section>

              {error && <div className="error-banner" role="alert"><WifiOff size={17} /><span>{error}</span><button onClick={() => setError(null)} aria-label="Dismiss"><X size={16} /></button></div>}

              {accountPending ? (
                <section className="management-panel account-status">
                  {accountError ? (
                    <>
                      <h2>Your company workspace could not be loaded</h2>
                      <p role="alert">{accountError.message}</p>
                      <p>{accountError.retrying ? "MeshRMM will keep retrying." : "Resolve the problem, then try again."}</p>
                      <button className="secondary-button" onClick={() => accountLoad.current?.retry()}><RefreshCw size={16} /> Retry now</button>
                    </>
                  ) : (
                    <p role="status"><LoaderCircle size={16} className="spin" /> Loading your company workspace…</p>
                  )}
                </section>
              ) : view === "settings" ? (<div className="settings-page">
                    <div className="settings-categories" role="tablist" aria-label="Settings categories">
                      {SETTINGS_TABS.map((tab, index) => (
                        <button
                          key={tab.id}
                          type="button"
                          role="tab"
                          id={`settings-tab-${tab.id}`}
                          aria-controls={tab.id}
                          aria-selected={settingsTab === tab.id}
                          tabIndex={settingsTab === tab.id ? 0 : -1}
                          onClick={() => setSettingsTab(tab.id)}
                          onKeyDown={(event) => {
                            let next: number;
                            if (event.key === "ArrowRight") next = (index + 1) % SETTINGS_TABS.length;
                            else if (event.key === "ArrowLeft") next = (index + SETTINGS_TABS.length - 1) % SETTINGS_TABS.length;
                            else if (event.key === "Home") next = 0;
                            else if (event.key === "End") next = SETTINGS_TABS.length - 1;
                            else return;
                            event.preventDefault();
                            setSettingsTab(SETTINGS_TABS[next].id);
                            document.getElementById(`settings-tab-${SETTINGS_TABS[next].id}`)?.focus();
                          }}
                        >{tab.label}</button>
                      ))}
                    </div>{!account?.company ? <p role="status">Loading company settings…</p> : <>{!isAdmin && <p className="session-notice">Company settings are managed by your administrator.</p>}<form className="company-settings-form" onSubmit={saveSessionPolicy}>
                    <fieldset disabled={!isAdmin || isSavingSessionPolicy}>
                    <section className="settings-section" id="dashboard-security" role="tabpanel" aria-labelledby="settings-tab-dashboard-security" hidden={settingsTab !== "dashboard-security"} tabIndex={0}>
                    <h2>Dashboard security</h2>
                    <p>Choose when an inactive dashboard session is paused.</p>
                    <label htmlFor="idle-timeout">Sign out inactive dashboards after<select id="idle-timeout" value={idleTimeoutDraft} onChange={(event) => setIdleTimeoutDraft(Number(event.target.value))}>
                    <option value={5}>5 minutes</option>
                    <option value={15}>15 minutes</option>
                    <option value={30}>30 minutes</option>
                    <option value={60}>1 hour</option>
                    <option value={120}>2 hours</option>
                    <option value={240}>4 hours</option>
                    <option value={480}>8 hours</option>
                    <option value={720}>12 hours</option>
                    <option value={1440}>24 hours</option>
                    </select>
                    </label>
                    </section>
                    <section className="settings-section" id="remote-sessions" role="tabpanel" aria-labelledby="settings-tab-remote-sessions" hidden={settingsTab !== "remote-sessions"} tabIndex={0}>
                    <h2>Remote sessions</h2>
                    <p>Defaults for new connections. Monitor highlighting can be changed in the viewer.</p>
                    <label>
                    <input type="checkbox" checked={displayBorderDraft} onChange={(event) => setDisplayBorderDraft(event.target.checked)} /> Highlight the viewed monitor on the agent’s physical display by default</label>
                    <label>
                    <input type="checkbox" checked={preventIdleDraft} onChange={(event) => setPreventIdleDraft(event.target.checked)} /> Prevent remote devices from locking while idle by default</label>
                    <label>
                    <input type="checkbox" checked={allowIdleOverrideDraft} onChange={(event) => setAllowIdleOverrideDraft(event.target.checked)} /> Allow users to change idle-lock prevention per session</label>
                    <p>The per-session permission above applies only to idle-lock prevention.</p>
                    </section>
                    <section className="settings-section" id="blackout" role="tabpanel" aria-labelledby="settings-tab-blackout" hidden={settingsTab !== "blackout"} tabIndex={0}>
                    <h2>Blackout message</h2>
                    <p>Shown on the remote device when a technician enables screen blackout.</p>
                    <label htmlFor="blackout-message">Agent blackout message<textarea id="blackout-message" rows={4} required maxLength={2048} value={blackoutMessageDraft} onChange={(event) => setBlackoutMessageDraft(event.target.value)} aria-describedby="blackout-message-help" />
                    </label>
                    <p id="blackout-message-help">Use {"{user_name}"} for the technician’s banner name. Applies to new remote sessions. Keep the message short (up to 2 KB).</p>
                    <div className="blackout-preview" aria-label="Blackout message preview">{blackoutMessageDraft.replaceAll("{user_name}", displayName)}</div>
                    <button type="button" className="secondary-button" onClick={() => setBlackoutMessageDraft(DEFAULT_BLACKOUT_MESSAGE)}>Restore default message</button>
                    </section>
                    </fieldset>
                    {settingsTab !== "blackout" && (!blackoutMessageDraft.trim() || new TextEncoder().encode(blackoutMessageDraft).length > 2048) && <p role="alert">Check the blackout message before saving: it must contain text and be no larger than 2 KB.</p>}
                    {isAdmin && <div className="settings-save">
                    <button className="primary-button" disabled={isSavingSessionPolicy || !blackoutMessageDraft.trim() || new TextEncoder().encode(blackoutMessageDraft).length > 2048 || (preventIdleDraft === (account?.company?.prevent_idle_lock ?? true) && allowIdleOverrideDraft === (account?.company?.allow_idle_override ?? true) && displayBorderDraft === (account?.company?.display_border ?? true) && idleTimeoutDraft === idleTimeoutMinutes && blackoutMessageDraft === (account?.company?.blackout_message ?? DEFAULT_BLACKOUT_MESSAGE))}>{isSavingSessionPolicy ? <LoaderCircle size={16} className="spin" /> : <Clock3 size={16} />} Save company settings</button>
                    </div>}{settingsNotice && <p role="status" className="session-notice">{settingsNotice}</p>}</form>
                    </>}</div>) : view === "agents" ? (
                <>
                {sessionNotice && <div className="session-notice" role="status">{sessionNotice}</div>}
                <AgentOverview
                    agents={agents}
                    filteredAgents={filteredAgents}
                    isLive={isLive}
                    lastUpdated={lastUpdated}
                    query={query}
                    status={status}
                    connectingId={connectingId}
                    connectingBackgroundId={connectingBackgroundId}
                    deletingId={deletingId}
                    closingId={closingId}
                    canDelete={isAdmin}
                    onQueryChange={setQuery}
                    onStatusChange={setStatus}
                    onRemote={(agent) => void remoteInto(agent)}
                    onRemoteBackground={(agent) => void remoteInto(agent, true)}
                    onCloseSession={(agent) => void closeAgentSession(agent)}
                    onDelete={(agent) => void deleteAgent(agent)}
                  />
                </>
                ) : (view === "team" || view === "sso") && !isAdmin ? (
                <section className="management-panel"><p>Only company administrators can manage users and authentication.</p></section>
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

      {isAuthOpen && <div className="modal-backdrop" role="presentation" onMouseDown={(event) => event.target === event.currentTarget && setIsAuthOpen(false)}><section className="settings-modal" role="dialog" aria-modal="true" aria-labelledby="account-title"><button className="modal-close" onClick={() => setIsAuthOpen(false)} aria-label="Close"><X size={19} /></button><div className="modal-icon"><ShieldCheck size={22} /></div><p className="eyebrow">Authenticated</p><h2 id="account-title">Your account</h2>{user ? <><div className="account-summary"><div className="profile-avatar">{initials}</div><div><strong>{displayName}</strong><span>{user.email}</span></div></div><button className="secondary-button modal-submit" onClick={() => void handleSignOut()}><LogOut size={16} /> Sign out</button></> : <button className="primary-button modal-submit" onClick={() => void signIn({ organizationId: workosOrganizationId, state: { returnTo: "/" } })}><ShieldCheck size={16} /> Sign in securely</button>}</section></div>}

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
