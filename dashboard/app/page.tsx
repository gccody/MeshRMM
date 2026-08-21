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
  Download,
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
import { FormEvent, useCallback, useEffect, useMemo, useRef, useState } from "react";
import { useRuntimeConfig } from "./providers";

type Agent = { id: string; name: string; connected: boolean };
type AgentSnapshot = {
  type: "snapshot";
  revision: number;
  agents: Agent[];
  generated_at_unix_ms: number;
};
type AgentEvent =
  | AgentSnapshot
  | { type: "agent_upsert"; revision: number; agent: Agent }
  | { type: "agent_deleted"; revision: number; agent_id: string };
type AgentDelta = Exclude<AgentEvent, AgentSnapshot>;
type AgentEventSubscription = {
  subscription_token: string;
  websocket_url: string;
  expires_at_unix_ms: number;
};
type Company = { id: string; name: string };
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
type AgentPlatform = "windows-x64";
type View = "agents" | "team" | "sso";

const INSTALLER_ASSETS: Record<AgentPlatform, { label: string; binary: string; checksum: string }> = {
  "windows-x64": {
    label: "Windows 10/11 (x64)",
    binary: "/downloads/pulsermm-agent-windows-x64.exe",
    checksum: "/downloads/pulsermm-agent-windows-x64.exe.sha256",
  },
};
const ENROLLMENT_MAGIC = "PULSERMM-BOOTSTRAP-V1";
const MAX_EVENT_RECONNECT_DELAY_MS = 30_000;

class AuthenticationRedirectStarted extends Error {}

const normalizeServer = (server: string) => server.trim().replace(/\/+$/, "");

const sortAgents = (items: Agent[]) => [...items].sort((left, right) =>
  Number(right.connected) - Number(left.connected)
  || left.name.localeCompare(right.name)
  || left.id.localeCompare(right.id));

const applyAgentDelta = (items: Agent[], event: AgentDelta) => event.type === "agent_upsert"
  ? sortAgents([...items.filter((agent) => agent.id !== event.agent.id), event.agent])
  : items.filter((agent) => agent.id !== event.agent_id);

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
  const [agentPlatform, setAgentPlatform] = useState<AgentPlatform>("windows-x64");
  const [isSaving, setIsSaving] = useState(false);
  const [isDownloadingInstaller, setIsDownloadingInstaller] = useState(false);
  const [installerDownloaded, setInstallerDownloaded] = useState(false);
  const [installerError, setInstallerError] = useState<string | null>(null);
  const agentRevision = useRef(-1);

  const isAdmin = role === "admin" || roles?.includes("admin") || account?.role === "admin" || account?.roles.includes("admin");
  const companyId = account?.company?.id;

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
      agentRevision.current = -1;
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
      const data = (await response.json()) as { agents?: Agent[]; revision?: number };
      const revision = Number.isSafeInteger(data.revision) ? data.revision! : -1;
      if (revision < agentRevision.current) return true;
      agentRevision.current = revision;
      setAgents(sortAgents(Array.isArray(data.agents) ? data.agents : []));
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
    if (!user || !organizationId || !companyId) return;
    agentRevision.current = -1;
    let disposed = false;
    let socket: WebSocket | null = null;
    let reconnectTimer: number | undefined;
    let reconnectDelay = 1_000;
    let awaitingSnapshot = true;
    let pendingEvents: AgentDelta[] = [];

    const scheduleReconnect = () => {
      if (disposed || reconnectTimer !== undefined) return;
      reconnectTimer = window.setTimeout(() => {
        reconnectTimer = undefined;
        void connect();
      }, reconnectDelay);
      reconnectDelay = Math.min(reconnectDelay * 2, MAX_EVENT_RECONNECT_DELAY_MS);
    };

    const connect = async () => {
      try {
        const response = await authorizedFetch("/v1/agents/events/subscriptions", { method: "POST" });
        if (!response.ok) {
          throw new Error(await errorMessage(response, "The live Agent event stream could not be opened."));
        }
        const subscription = (await response.json()) as AgentEventSubscription;
        if (disposed) return;
        const websocketUrl = new URL(subscription.websocket_url);
        websocketUrl.searchParams.set("token", subscription.subscription_token);
        const nextSocket = new WebSocket(websocketUrl);
        socket = nextSocket;
        awaitingSnapshot = true;
        pendingEvents = [];

        const requestSnapshot = () => {
          if (nextSocket.readyState === WebSocket.OPEN) nextSocket.send("refresh");
        };

        nextSocket.addEventListener("open", () => {
          if (disposed || socket !== nextSocket) return;
          reconnectDelay = 1_000;
          setIsLive(true);
          setError(null);
        });
        nextSocket.addEventListener("message", (message) => {
          if (disposed || socket !== nextSocket || typeof message.data !== "string") return;
          try {
            const event = JSON.parse(message.data) as AgentEvent;
            if (!Number.isSafeInteger(event.revision) || event.revision < 0) return;
            if (event.type === "snapshot") {
              if (!Array.isArray(event.agents)) return;
              if (event.revision < agentRevision.current) {
                requestSnapshot();
                return;
              }
              let nextAgents = sortAgents(event.agents);
              let nextRevision = event.revision;
              for (const pending of pendingEvents.sort((left, right) => left.revision - right.revision)) {
                if (pending.revision <= nextRevision) continue;
                if (pending.revision !== nextRevision + 1) {
                  pendingEvents = [];
                  awaitingSnapshot = true;
                  requestSnapshot();
                  return;
                }
                nextAgents = applyAgentDelta(nextAgents, pending);
                nextRevision = pending.revision;
              }
              pendingEvents = [];
              awaitingSnapshot = false;
              agentRevision.current = nextRevision;
              setAgents(nextAgents);
            } else {
              if (event.type !== "agent_upsert" && event.type !== "agent_deleted") return;
              if (awaitingSnapshot) {
                pendingEvents.push(event);
                if (pendingEvents.length > 1_000) nextSocket.close(1009, "too many pending Agent events");
                return;
              }
              if (event.revision <= agentRevision.current) return;
              if (event.revision !== agentRevision.current + 1) {
                awaitingSnapshot = true;
                pendingEvents = [event];
                requestSnapshot();
                return;
              }
              agentRevision.current = event.revision;
              setAgents((current) => applyAgentDelta(current, event));
            }
            setIsLive(true);
            setLastUpdated(new Date());
          } catch {
            requestSnapshot();
          }
        });
        nextSocket.addEventListener("error", () => nextSocket.close());
        nextSocket.addEventListener("close", () => {
          if (disposed || socket !== nextSocket) return;
          socket = null;
          setIsLive(false);
          scheduleReconnect();
        });
      } catch (requestError) {
        if (disposed || requestError instanceof AuthenticationRedirectStarted) return;
        setIsLive(false);
        setError(requestError instanceof Error ? requestError.message : "The live Agent event stream could not be opened.");
        scheduleReconnect();
      }
    };

    void connect();
    return () => {
      disposed = true;
      if (reconnectTimer !== undefined) window.clearTimeout(reconnectTimer);
      socket?.close(1000, "dashboard subscription ended");
    };
  }, [authorizedFetch, companyId, organizationId, user]);

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
                  {isAdmin && <button className="primary-button" onClick={() => { setAgentPlatform("windows-x64"); setInstallerDownloaded(false); setInstallerError(null); setIsAgentOpen(true); }}><Plus size={16} /> Add Agent</button>}
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
                    <div className="panel-footer"><span>Updates arrive in real time</span><span className={isLive ? "" : "disconnected"}><i /> {isLive ? "Tenant-scoped event stream ready" : "Reconnecting event stream"}</span></div>
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

      {isAgentOpen && (
        <div className="modal-backdrop" role="presentation" onMouseDown={(event) => event.target === event.currentTarget && setIsAgentOpen(false)}>
          <section className="settings-modal enrollment-modal" role="dialog" aria-modal="true" aria-labelledby="agent-title">
            <button className="modal-close" onClick={() => setIsAgentOpen(false)} aria-label="Close"><X size={19} /></button>
            <div className="modal-icon"><Monitor size={22} /></div>
            <p className="eyebrow">{account?.company?.name}</p>
            <h2 id="agent-title">Download Agent installer</h2>
            <p>Select the endpoint platform and download one installer. Setup will use the Windows computer name automatically, generate the device ID on the server, configure the Agent, and install the LocalSystem service.</p>
            <form onSubmit={downloadInstaller}>
              <label>Installer platform<select required value={agentPlatform} onChange={(event) => setAgentPlatform(event.target.value as AgentPlatform)}><option value="windows-x64">Windows 10/11 (x64)</option></select><small className="field-help">The Agent currently supports 64-bit Windows endpoints.</small></label>
              <div className="installer-summary">
                <div><Monitor size={18} /><span><strong>Automatic machine identity</strong><small>Computer name from Windows · server-generated device ID</small></span></div>
                <div><ShieldCheck size={18} /><span><strong>Administrator installation</strong><small>Automatic LocalSystem service with recovery</small></span></div>
              </div>
              {installerError && <div className="installer-error" role="alert">{installerError}</div>}
              <button className="primary-button modal-submit" disabled={isDownloadingInstaller}>
                {isDownloadingInstaller ? <LoaderCircle size={16} className="spin" /> : installerDownloaded ? <Check size={16} /> : <Download size={16} />}
                {isDownloadingInstaller ? "Preparing installer..." : installerDownloaded ? "Download another installer" : "Download installer"}
              </button>
              <p className="installer-secret-note">Run the downloaded EXE and approve the Windows User Account Control prompt. Its enrollment authorization expires after 30 minutes and can only be used once.</p>
            </form>
          </section>
        </div>
      )}
    </div>
  );
}
