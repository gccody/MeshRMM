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
import Link from "next/link";
import { type ReactNode, useCallback, useEffect, useMemo, useRef, useState } from "react";
import { AUTH_REFRESH_FAILED_EVENT, useRuntimeConfig } from "../../app/providers";
import { AgentOverview, type AgentStatusFilter } from "../agents/agent-overview";
import { useAgentInventory } from "../agents/use-agent-inventory";
import type { Agent } from "../agents/types";
import { EnrollmentModal } from "../enrollment/enrollment-modal";
import { useInstallerDownload } from "../enrollment/use-installer-download";
import { AuthenticationRequired, errorMessage, normalizeServer } from "../../lib/http";
import {
  DEFAULT_IDLE_TIMEOUT_MINUTES,
  formatIdleTimeout,
} from "../session/idle-session";
import { useRemoteHandoff } from "../session/use-remote-handoff";
import { SettingsPage } from "../settings/settings-page";
import { AccountLoadError, accountLoader } from "./account-load";
import { AccountModal } from "./account-modal";
import type { Account } from "./types";
import { useIdleSession } from "../session/use-idle-session";
import { MarketingPage } from "../marketing/marketing-page";
import { PlatformDashboard } from "../platform/platform-dashboard";

type View = "agents" | "team" | "sso" | "settings";
const VIEW_PATHS: Record<View, string> = { agents: "/", team: "/users", sso: "/authentication", settings: "/settings" };
type SessionPauseReason = "idle" | "expired";

export default function Dashboard({ view = "agents" }: { view?: View }) {
  const { surface } = useRuntimeConfig();
  if (surface === "marketing") return <MarketingPage />;
  if (surface === "platform") return <PlatformDashboard />;
  return <TenantDashboard view={view} />;
}

function TenantDashboard({ view }: { view: View }) {
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
  const [query, setQuery] = useState("");
  const [status, setStatus] = useState<AgentStatusFilter>("all");
  const [isAuthOpen, setIsAuthOpen] = useState(false);
  const [isAgentOpen, setIsAgentOpen] = useState(false);
  const [isSidebarOpen, setIsSidebarOpen] = useState(false);
  // The control that opened the account or enrollment dialog gets focus back.
  const dialogOpener = useRef<HTMLElement | null>(null);
  const [deletingId, setDeletingId] = useState<string | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [sessionPauseReason, setSessionPauseReason] = useState<SessionPauseReason | null>(null);
  const [isResumingSession, setIsResumingSession] = useState(false);

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

  const installer = useInstallerDownload(authorizedFetch);
  const remote = useRemoteHandoff({ authorizedFetch, reportError: setError });

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
        setAccount(data);
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
  }, [fetchAccount, hasTenantSession, isAuthLoading, sessionPauseReason]);

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

  const closeSidebar = () => setIsSidebarOpen(false);
  const signInToCompany = () => void signIn({ organizationId: workosOrganizationId, state: { returnTo: "/" } });

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
          <NavItem view="agents" current={view} disabled={Boolean(sessionPauseReason)} onNavigate={closeSidebar}><Monitor size={18} /><span>Devices</span>{isLive ? <em>{agents.length}</em> : null}</NavItem>
          {isAdmin && <NavItem view="team" current={view} disabled={!hasTenantSession || Boolean(sessionPauseReason)} onNavigate={closeSidebar}><Users size={18} /><span>Users</span></NavItem>}
          {isAdmin && <NavItem view="sso" current={view} disabled={!hasTenantSession || Boolean(sessionPauseReason)} onNavigate={closeSidebar}><KeyRound size={18} /><span>Authentication</span></NavItem>}
          <NavItem view="settings" current={view} disabled={!hasTenantSession || Boolean(sessionPauseReason)} onNavigate={closeSidebar}><Settings size={18} /><span>Settings</span></NavItem>
        </nav>

        <button className="profile-row profile-button" onClick={(event) => { dialogOpener.current = event.currentTarget; setIsSidebarOpen(false); setIsAuthOpen(true); }} disabled={Boolean(sessionPauseReason)} aria-haspopup="dialog">
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
              <button className="primary-button" onClick={signInToCompany}><ShieldCheck size={16} /> Sign in securely</button>
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
                  {isAdmin && <button className="primary-button" onClick={(event) => { dialogOpener.current = event.currentTarget; installer.reset(); setIsAgentOpen(true); }} aria-haspopup="dialog"><Plus size={16} /> Add device</button>}
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
              ) : view === "settings" ? (
                <SettingsPage
                  company={account?.company}
                  isAdmin={isAdmin}
                  displayName={displayName}
                  authorizedFetch={authorizedFetch}
                  onSaved={setAccount}
                  reportError={setError}
                />
              ) : view === "agents" ? (
                <>
                {remote.sessionNotice && <div className="session-notice" role="status">{remote.sessionNotice}</div>}
                <AgentOverview
                    agents={agents}
                    filteredAgents={filteredAgents}
                    isLive={isLive}
                    lastUpdated={lastUpdated}
                    query={query}
                    status={status}
                    connectingId={remote.connectingId}
                    connectingBackgroundId={remote.connectingBackgroundId}
                    deletingId={deletingId}
                    closingId={remote.closingId}
                    canDelete={isAdmin}
                    onQueryChange={setQuery}
                    onStatusChange={setStatus}
                    onRemote={(agent) => void remote.connect(agent)}
                    onRemoteBackground={(agent) => void remote.connect(agent, true)}
                    onCloseSession={(agent) => void remote.closeSession(agent)}
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

      {isAuthOpen && (
        <AccountModal
          email={user?.email ?? null}
          displayName={displayName}
          initials={initials}
          onClose={() => setIsAuthOpen(false)}
          onSignIn={signInToCompany}
          onSignOut={() => void handleSignOut()}
          returnFocus={dialogOpener}
        />
      )}

      {isAgentOpen && (
        <EnrollmentModal
          companyName={account?.company?.name}
          platform={installer.platform}
          error={installer.error}
          isDownloading={installer.isDownloading}
          downloaded={installer.downloaded}
          onClose={() => setIsAgentOpen(false)}
          onPlatformChange={installer.setPlatform}
          onSubmit={(event) => void installer.download(event)}
          returnFocus={dialogOpener}
        />
      )}
    </div>
  );
}

// Sidebar entries are links so they can open in a new tab. A plain click
// navigates in place and closes the mobile sidebar. Unavailable entries stay
// disabled buttons.
function NavItem({ view, current, disabled, onNavigate, children }: {
  view: View;
  current: View;
  disabled: boolean;
  onNavigate: () => void;
  children: ReactNode;
}) {
  const className = `nav-item ${view === current ? "active" : ""}`;
  const ariaCurrent = view === current ? "page" : undefined;
  if (disabled) {
    return <button type="button" className={className} aria-current={ariaCurrent} disabled>{children}</button>;
  }
  return <Link href={VIEW_PATHS[view]} prefetch={false} className={className} aria-current={ariaCurrent} onNavigate={onNavigate}>{children}</Link>;
}
