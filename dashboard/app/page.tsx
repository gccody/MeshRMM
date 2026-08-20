"use client";

import { useAuth } from "@workos-inc/authkit-react";
import {
  AdminPortalDomainVerification,
  AdminPortalSsoConnection,
  OrganizationSwitcher,
  UsersManagement,
} from "@workos-inc/widgets";
import {
  Activity,
  Building2,
  Check,
  ChevronDown,
  Clock3,
  Copy,
  Filter,
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
  Wifi,
  WifiOff,
  X,
} from "lucide-react";
import { FormEvent, useCallback, useEffect, useMemo, useState } from "react";
import { useRuntimeConfig } from "./providers";

type Agent = { id: string; name: string; connected: boolean };
type Company = { id: string; name: string };
type Account = {
  user_id: string;
  company: Company | null;
  role: string | null;
  roles: string[];
  permissions: string[];
};
type AgentConfig = {
  server: string;
  device_id: string;
  agent_token: string;
  frames_per_second: number;
  bitrate_bits_per_second: number;
  json_logs: boolean;
};
type Enrollment = { agent: Agent; config: AgentConfig };
type View = "agents" | "team" | "sso";

class AuthenticationRedirectStarted extends Error {}

const normalizeServer = (server: string) => server.trim().replace(/\/+$/, "");

async function errorMessage(response: Response, fallback: string) {
  try {
    const body = (await response.json()) as { error?: string };
    return body.error || fallback;
  } catch {
    return fallback;
  }
}

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
  const [agents, setAgents] = useState<Agent[]>([]);
  const [isLive, setIsLive] = useState(false);
  const [query, setQuery] = useState("");
  const [status, setStatus] = useState<"all" | "online" | "offline">("all");
  const [isRefreshing, setIsRefreshing] = useState(false);
  const [isAuthOpen, setIsAuthOpen] = useState(false);
  const [isOrganizationOpen, setIsOrganizationOpen] = useState(false);
  const [isAgentOpen, setIsAgentOpen] = useState(false);
  const [isSidebarOpen, setIsSidebarOpen] = useState(false);
  const [connectingId, setConnectingId] = useState<string | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [lastUpdated, setLastUpdated] = useState<Date>(new Date());
  const [companyName, setCompanyName] = useState("");
  const [agentId, setAgentId] = useState("");
  const [agentName, setAgentName] = useState("");
  const [isSaving, setIsSaving] = useState(false);
  const [enrollment, setEnrollment] = useState<Enrollment | null>(null);
  const [copied, setCopied] = useState(false);

  const isAdmin = role === "admin" || roles?.includes("admin") || account?.role === "admin" || account?.roles.includes("admin");

  const authorizedFetch = useCallback(async (path: string, init: RequestInit = {}) => {
    let token: string;
    try {
      token = await getAccessToken();
    } catch (tokenError) {
      if (tokenError instanceof Error && tokenError.message === "No access token available") {
        void signIn({ organizationId: organizationId ?? undefined, state: { returnTo: "/" } });
        throw new AuthenticationRedirectStarted();
      }
      throw tokenError;
    }
    if (!token) throw new Error("Your WorkOS session has expired. Please sign in again.");
    return fetch(`${normalizeServer(serverUrl)}${path}`, {
      ...init,
      headers: { ...init.headers, Authorization: `Bearer ${token}` },
    });
  }, [getAccessToken, organizationId, serverUrl, signIn]);

  const loadAccount = useCallback(async () => {
    if (!user || !organizationId) {
      setAccount(null);
      setAgents([]);
      setIsLive(false);
      return null;
    }
    const response = await authorizedFetch("/v1/account");
    if (!response.ok) throw new Error(await errorMessage(response, "The company account could not be loaded."));
    const data = (await response.json()) as Account;
    setAccount(data);
    return data;
  }, [authorizedFetch, organizationId, user]);

  const loadAgents = useCallback(async (silent = false) => {
    if (!user || !organizationId) return false;
    if (!silent) setIsRefreshing(true);
    setError(null);
    try {
      const response = await authorizedFetch("/v1/agents");
      if (!response.ok) throw new Error(await errorMessage(response, "The live agent service could not be reached."));
      const data = (await response.json()) as { agents?: Agent[] };
      setAgents(Array.isArray(data.agents) ? data.agents : []);
      setIsLive(true);
      setLastUpdated(new Date());
      return true;
    } catch (requestError) {
      if (requestError instanceof AuthenticationRedirectStarted) return false;
      setIsLive(false);
      setAgents([]);
      setError(requestError instanceof Error ? requestError.message : "The live agent service could not be reached.");
      return false;
    } finally {
      setIsRefreshing(false);
    }
  }, [authorizedFetch, organizationId, user]);

  useEffect(() => {
    if (isAuthLoading || !user || !organizationId) return;
    let cancelled = false;
    const start = async () => {
      try {
        const loaded = await loadAccount();
        if (!cancelled && loaded?.company) await loadAgents();
      } catch (requestError) {
        if (!cancelled && !(requestError instanceof AuthenticationRedirectStarted)) {
          setError(requestError instanceof Error ? requestError.message : "The company account could not be loaded.");
        }
      }
    };
    void start();
    return () => { cancelled = true; };
  }, [isAuthLoading, loadAccount, loadAgents, organizationId, user]);

  useEffect(() => {
    if (!user || !organizationId || !account?.company) return;
    const timer = window.setInterval(() => void loadAgents(true), 15_000);
    return () => window.clearInterval(timer);
  }, [account?.company, loadAgents, organizationId, user]);

  const filteredAgents = useMemo(() => {
    const search = query.trim().toLowerCase();
    return agents.filter((agent) => {
      const matchesSearch = !search || `${agent.name} ${agent.id}`.toLowerCase().includes(search);
      const matchesStatus = status === "all" || (status === "online" ? agent.connected : !agent.connected);
      return matchesSearch && matchesStatus;
    });
  }, [agents, query, status]);

  const online = agents.filter((agent) => agent.connected).length;
  const offline = agents.length - online;
  const coverage = agents.length ? Math.round((online / agents.length) * 100) : 0;
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
      setCompanyName("");
      await loadAgents();
    } catch (requestError) {
      setError(requestError instanceof Error ? requestError.message : "The company could not be provisioned.");
    } finally {
      setIsSaving(false);
    }
  };

  const createAgent = async (event: FormEvent) => {
    event.preventDefault();
    setIsSaving(true);
    setError(null);
    try {
      const response = await authorizedFetch("/v1/agents", {
        method: "POST",
        headers: { "Content-Type": "application/json" },
        body: JSON.stringify({ id: agentId, name: agentName }),
      });
      if (!response.ok) throw new Error(await errorMessage(response, "The Agent could not be created."));
      const data = (await response.json()) as Enrollment;
      setEnrollment(data);
      setAgentId("");
      setAgentName("");
      await loadAgents();
    } catch (requestError) {
      setError(requestError instanceof Error ? requestError.message : "The Agent could not be created.");
    } finally {
      setIsSaving(false);
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

  const copyEnrollment = async () => {
    if (!enrollment) return;
    await navigator.clipboard.writeText(JSON.stringify(enrollment.config, null, 2));
    setCopied(true);
    window.setTimeout(() => setCopied(false), 1800);
  };

  const handleSignOut = () => {
    setAgents([]);
    setAccount(null);
    setIsLive(false);
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

        <button className="workspace-switcher" onClick={() => setIsOrganizationOpen(true)} disabled={!user}>
          <div className="workspace-avatar">{account?.company?.name.slice(0, 2).toUpperCase() ?? "CO"}</div>
          <div><strong>{companyLabel}</strong><span>{organizationId ? "WorkOS organization" : "Organization required"}</span></div>
          <ChevronDown size={16} />
        </button>

        <nav aria-label="Primary navigation">
          <p className="nav-label">Company</p>
          <button className={`nav-item ${view === "agents" ? "active" : ""}`} onClick={() => setActiveView("agents")}><Monitor size={18} /><span>Agents</span>{isLive ? <em>{agents.length}</em> : null}</button>
          <button className={`nav-item ${view === "team" ? "active" : ""}`} onClick={() => setActiveView("team")} disabled={!organizationId}><Users size={18} /><span>Users</span></button>
          <button className={`nav-item ${view === "sso" ? "active" : ""}`} onClick={() => setActiveView("sso")} disabled={!organizationId}><KeyRound size={18} /><span>Single sign-on</span></button>
          <p className="nav-label nav-label-spaced">Account</p>
          <button className="nav-item" onClick={() => setIsAuthOpen(true)}><Settings size={18} /><span>Profile & session</span></button>
        </nav>

        <button className="profile-row profile-button" onClick={() => setIsAuthOpen(true)}>
          <div className="profile-avatar">{initials}</div>
          <div><strong>{displayName}</strong><span>{user?.email ?? "WorkOS authentication"}</span></div>
        </button>
      </aside>

      {isSidebarOpen && <button className="sidebar-scrim" onClick={() => setIsSidebarOpen(false)} aria-label="Close navigation" />}

      <main className="main-content">
        <header className="topbar">
          <button className="mobile-menu" onClick={() => setIsSidebarOpen(true)} aria-label="Open navigation"><Menu size={21} /></button>
          {view === "agents" ? <label className="global-search"><Search size={18} /><input value={query} onChange={(event) => setQuery(event.target.value)} aria-label="Search Agents" placeholder="Search Agents by name or device ID..." /></label> : <div />}
          <div className="topbar-actions">
            <button className="connection-pill" onClick={() => setIsAuthOpen(true)}>
              <span className={`pulse-dot ${isLive ? "live" : ""}`} />
              {isAuthLoading ? "Checking session" : isLive ? "Service live" : user ? "Signed in" : "Sign in"}
            </button>
          </div>
        </header>

        <div className="page-wrap">
          {!user && !isAuthLoading ? (
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
                  {isAdmin && <button className="primary-button" onClick={() => { setEnrollment(null); setIsAgentOpen(true); }}><Plus size={16} /> Add Agent</button>}
                </div>}
              </section>

              {error && <div className="error-banner"><WifiOff size={17} /><span>{error}</span><button onClick={() => setError(null)} aria-label="Dismiss"><X size={16} /></button></div>}

              {view === "agents" ? (
                <>
                  <section className="metrics-grid" aria-label="Agent summary">
                    <article className="metric-card"><div className="metric-icon purple"><Monitor size={19} /></div><div><span>Total Agents</span><strong>{isLive ? agents.length : "--"}</strong><small>{isLive ? "Company-owned endpoints" : "Awaiting live service"}</small></div></article>
                    <article className="metric-card"><div className="metric-icon green"><Wifi size={19} /></div><div><span>Online now</span><strong>{isLive ? online : "--"}</strong><small>{isLive ? <><b>{coverage}%</b> connection rate</> : "Awaiting live service"}</small></div>{isLive ? <span className="metric-badge good">Live</span> : null}</article>
                    <article className="metric-card"><div className="metric-icon amber"><WifiOff size={19} /></div><div><span>Offline</span><strong>{isLive ? offline : "--"}</strong><small>{isLive ? "Current signaling state" : "Awaiting live service"}</small></div>{isLive ? <span className={`metric-badge ${offline ? "warn" : "good"}`}>{offline ? "Review" : "Clear"}</span> : null}</article>
                    <article className="metric-card"><div className="metric-icon blue"><Clock3 size={19} /></div><div><span>Inventory status</span><strong className="status-word">{isLive ? "Live" : "Unavailable"}</strong><small>{isLive ? `Updated ${lastUpdated.toLocaleTimeString([], { hour: "numeric", minute: "2-digit" })}` : "No live response"}</small></div>{isLive ? <Check size={20} className="metric-check" /> : null}</article>
                  </section>

                  <section className="agent-panel">
                    <div className="panel-header"><div><h2>Company Agents</h2><span>{filteredAgents.length} of {agents.length} devices</span></div></div>
                    <div className="table-toolbar">
                      <label className="agent-search"><Search size={17} /><input value={query} onChange={(event) => setQuery(event.target.value)} placeholder="Search by name or device ID..." /></label>
                      <div className="status-filter"><Filter size={16} /><select value={status} onChange={(event) => setStatus(event.target.value as typeof status)} aria-label="Filter by status"><option value="all">All statuses</option><option value="online">Online</option><option value="offline">Offline</option></select><ChevronDown size={14} /></div>
                    </div>
                    <div className="agent-table" role="table" aria-label="Managed Agents">
                      <div className="table-head" role="row"><span>Device</span><span>Connection</span><span>Device ID</span><span aria-hidden="true" /></div>
                      {filteredAgents.map((agent) => (
                        <div className="agent-row" role="row" key={agent.id}>
                          <div className="device-cell"><div className={`device-avatar ${agent.connected ? "online" : ""}`}>{(agent.name.match(/[a-z0-9]/i)?.[0] ?? "A").toUpperCase()}<span /></div><div><strong>{agent.name}</strong><code>Cloudflare live inventory</code></div></div>
                          <div><span className={`status-badge ${agent.connected ? "online" : "offline"}`}><i />{agent.connected ? "Online" : "Offline"}</span></div>
                          <div className="device-id-cell"><code>{agent.id}</code></div>
                          <div className="row-actions"><button className={`remote-button ${!agent.connected ? "disabled" : ""}`} disabled={!agent.connected || connectingId === agent.id} onClick={() => void remoteInto(agent)}>{connectingId === agent.id ? <LoaderCircle size={15} className="spin" /> : <Monitor size={15} />}{connectingId === agent.id ? "Authorizing..." : "Remote"}</button></div>
                        </div>
                      ))}
                      {!filteredAgents.length && <div className="empty-state"><Search size={24} /><strong>{isLive ? "No Agents found" : "No live Agent data"}</strong><span>{isLive ? "Create an Agent or adjust the current filters." : "The company inventory has not returned a live response."}</span></div>}
                    </div>
                    <div className="panel-footer"><span>Auto-refreshes every 15 seconds</span><span className={isLive ? "" : "disconnected"}><i /> {isLive ? "Tenant-scoped signaling ready" : "Signaling unavailable"}</span></div>
                  </section>
                </>
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

      {isAuthOpen && <div className="modal-backdrop" role="presentation" onMouseDown={(event) => event.target === event.currentTarget && setIsAuthOpen(false)}><section className="settings-modal" role="dialog" aria-modal="true" aria-labelledby="account-title"><button className="modal-close" onClick={() => setIsAuthOpen(false)} aria-label="Close"><X size={19} /></button><div className="modal-icon"><ShieldCheck size={22} /></div><p className="eyebrow">Authenticated</p><h2 id="account-title">WorkOS account</h2><p>Your session carries the selected organization and role used for every PulseRMM API request.</p>{user ? <><div className="account-summary"><div className="profile-avatar">{initials}</div><div><strong>{displayName}</strong><span>{user.email}</span></div></div><button className="secondary-button modal-submit" onClick={handleSignOut}><LogOut size={16} /> Sign out</button></> : <button className="primary-button modal-submit" onClick={() => void signIn({ state: { returnTo: "/" } })}><ShieldCheck size={16} /> Continue with WorkOS</button>}</section></div>}

      {isOrganizationOpen && user && <div className="modal-backdrop" role="presentation" onMouseDown={(event) => event.target === event.currentTarget && setIsOrganizationOpen(false)}><section className="settings-modal organization-modal" role="dialog" aria-modal="true" aria-labelledby="organization-title"><button className="modal-close" onClick={() => setIsOrganizationOpen(false)} aria-label="Close"><X size={19} /></button><div className="modal-icon"><Building2 size={22} /></div><p className="eyebrow">Tenant boundary</p><h2 id="organization-title">Switch company</h2><p>Changing companies refreshes your WorkOS token before any other company inventory is requested.</p><div className="organization-widget"><OrganizationSwitcher authToken={getAccessToken} switchToOrganization={switchToOrganization} /></div></section></div>}

      {isAgentOpen && <div className="modal-backdrop" role="presentation" onMouseDown={(event) => event.target === event.currentTarget && !enrollment && setIsAgentOpen(false)}><section className={`settings-modal ${enrollment ? "enrollment-modal" : ""}`} role="dialog" aria-modal="true" aria-labelledby="agent-title"><button className="modal-close" onClick={() => { setEnrollment(null); setIsAgentOpen(false); }} aria-label="Close"><X size={19} /></button><div className="modal-icon"><Monitor size={22} /></div><p className="eyebrow">{enrollment ? "Credential issued once" : account?.company?.name}</p><h2 id="agent-title">{enrollment ? `${enrollment.agent.name} is ready to install` : "Create Agent"}</h2>{enrollment ? <><p>Copy this live enrollment configuration now. PulseRMM stores only its SHA-256 credential hash and cannot show the token again.</p><pre className="enrollment-config">{JSON.stringify(enrollment.config, null, 2)}</pre><button className="primary-button modal-submit" onClick={() => void copyEnrollment()}>{copied ? <Check size={16} /> : <Copy size={16} />}{copied ? "Copied" : "Copy agent.json"}</button></> : <><p>The device ID is globally unique. The Agent will only appear inside this company.</p><form onSubmit={createAgent}><label>Display name<input required maxLength={120} value={agentName} onChange={(event) => setAgentName(event.target.value)} placeholder="Reception workstation" /></label><label>Device ID<input required maxLength={128} pattern="[A-Za-z0-9._-]+" value={agentId} onChange={(event) => setAgentId(event.target.value)} placeholder="reception-ws-01" /></label><button className="primary-button modal-submit" disabled={isSaving}>{isSaving ? <LoaderCircle size={16} className="spin" /> : <Plus size={16} />} Create Agent</button></form></>}</section></div>}
    </div>
  );
}
