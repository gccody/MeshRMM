import {
  Building2,
  ClipboardList,
  Clock3,
  KeyRound,
  LoaderCircle,
  LogIn,
  LogOut,
  Menu,
  Monitor,
  Network,
  Plus,
  RefreshCw,
  Settings,
  ShieldCheck,
  ShieldAlert,
  UserCog,
  Users,
  Wrench,
  X,
} from "lucide-react";
import { type ReactNode, useCallback, useReducer, useRef, useState } from "react";
import { Link, Navigate, Outlet, useLocation } from "react-router";
import { AuthenticationRequired, apiFetch, errorMessage } from "../../lib/http";
import { useAgentInventory } from "../agents/use-agent-inventory";
import type { InventoryConnection } from "../agents/inventory-stream";
import type { Agent } from "../agents/types";
import { loginPath } from "../auth/next-path";
import { type LockReason, useDocumentTitle, useSession } from "../auth/session";
import type { Account, Permission } from "../auth/types";
import { EnrollmentModal } from "../enrollment/enrollment-modal";
import { useInstallerDownload } from "../enrollment/use-installer-download";
import { formatIdleTimeout } from "../session/idle-session";
import { useIdleSession } from "../session/use-idle-session";
import { useRemoteHandoff } from "../session/use-remote-handoff";
import { ViewerDownloadCard } from "../session/viewer-download-links";
import type { GeneralSettings } from "../settings/general-settings";
import { useSettingsDraft } from "../settings/use-settings-draft";
import { type ActionErrorSource, actionErrorsReducer } from "./action-errors";
import { VIEW_COPY, VIEW_PATHS, type View, canView, homeView, viewForPath } from "./views";
import { type Workspace, WorkspaceContext } from "./workspace-context";

// The topbar pill while the inventory on screen is out of date.
const CONNECTION_PILL: Record<InventoryConnection, string> = {
  connecting: "Reconnecting",
  live: "Connected",
  reconnecting: "Reconnecting",
  offline: "Offline",
};

const NAV_ICONS: Record<View, typeof Monitor> = {
  devices: Monitor,
  toolbox: Wrench,
  users: Users,
  roles: UserCog,
  authentication: KeyRound,
  settings: Settings,
  audit: ClipboardList,
  account: ShieldCheck,
};

const NAV_GROUPS: { label: string; views: View[] }[] = [
  { label: "Workspace", views: ["devices", "toolbox"] },
  { label: "Administration", views: ["users", "roles", "authentication", "settings", "audit"] },
];

// The frame around every workspace page. The layout route renders it, so it
// stays mounted while the user moves between pages: the live inventory, the
// idle timer and unsaved settings are not reloaded or reset. Until the
// browser knows who is signed in (and in the prerendered page), it shows the
// page's heading over a loading state.
export function WorkspaceShell() {
  const { instance, state } = useSession();
  const location = useLocation();
  const view = viewForPath(location.pathname) ?? "devices";
  useDocumentTitle(VIEW_COPY[view].title);

  if (instance?.setup_required) return <Navigate to="/setup" replace />;
  if (state.status === "signed-out" && state.reason === null) {
    return <Navigate to={loginPath(`${location.pathname}${location.search}`)} replace />;
  }
  if (state.status === "signed-in") return <SignedInWorkspace key={state.account.user.id} account={state.account} view={view} />;

  const pill = state.status === "loading" ? "Checking session" : state.status === "unavailable" ? "Unavailable" : "Session paused";
  return (
    <Frame instanceName={instance?.name} pill={pill}>
      {state.status === "signed-out" && state.reason !== null
        ? <PausedCard reason={state.reason} idleTimeoutMinutes={state.idleTimeoutMinutes ?? null} />
        : (
          <>
            <PageHeading view={view} />
            {state.status === "unavailable" ? <UnavailablePanel message={state.message} retrying={state.retrying} /> : <LoadingPanel />}
          </>
        )}
    </Frame>
  );
}

function SignedInWorkspace({ account, view }: { account: Account; view: View }) {
  const { instance, refresh, signOut, lock } = useSession();
  const location = useLocation();
  const [devicesSearch, setDevicesSearch] = useState("");
  const [isAgentOpen, setIsAgentOpen] = useState(false);
  const [isSidebarOpen, setIsSidebarOpen] = useState(false);
  // The control that opened the enrollment dialog gets focus back.
  const dialogOpener = useRef<HTMLElement | null>(null);
  const [deletingId, setDeletingId] = useState<string | null>(null);
  const [actionErrors, dispatchActionError] = useReducer(actionErrorsReducer, {});
  const [settings, setSettings] = useState<GeneralSettings | null>(null);
  const enrolling = account.two_factor.enrollment_required;
  const can = useCallback((permission: Permission) => account.permissions.includes(permission), [account]);

  const refreshAccount = useCallback(async () => {
    await refresh();
  }, [refresh]);

  const authorizedFetch = useCallback(async (path: string, init?: RequestInit) => {
    const response = await apiFetch(path, init);
    if (response.status === 401) {
      await response.body?.cancel();
      lock("expired");
      throw new AuthenticationRequired();
    }
    if (response.status === 403) {
      const body: unknown = await response.clone().json().catch(() => null);
      if (body && typeof body === "object" && "code" in body && body.code === "two_factor_enrollment_required") void refresh().catch(() => {});
    }
    return response;
  }, [lock, refresh]);

  const inventory = useAgentInventory({
    enabled: can("devices.view") && !enrolling,
    authorizedFetch,
    // A refused or revoked socket means the session or the user's access
    // changed; the account says which.
    onRefused: useCallback(() => void refresh().catch(() => {}), [refresh]),
  });
  const { agents, hasData, status: inventoryStatus, connection, isRefreshing, refresh: refreshInventory, reset: resetInventory } = inventory;
  const reportActionError = useCallback(
    (source: ActionErrorSource, message: string | null) => dispatchActionError({ source, message }),
    [],
  );
  const reportRemoteError = useCallback((message: string | null) => reportActionError("remote", message), [reportActionError]);
  const remote = useRemoteHandoff({ authorizedFetch, reportError: reportRemoteError });
  const installer = useInstallerDownload(authorizedFetch);
  const settingsDraft = useSettingsDraft(settings);

  useIdleSession({
    enabled: true,
    userId: account.user.id,
    timeoutMinutes: account.idle_timeout_minutes,
    onTimeout: useCallback(() => {
      resetInventory();
      void signOut("idle");
    }, [resetInventory, signOut]),
  });

  // Navigating closes the sidebar, so it doesn't cover the new page.
  const [sidebarPath, setSidebarPath] = useState(location.pathname);
  if (sidebarPath !== location.pathname) {
    setSidebarPath(location.pathname);
    setIsSidebarOpen(false);
  }

  const deleteAgent = async (agent: Agent) => {
    const confirmed = window.confirm(
      `Delete ${agent.name}? The Agent will uninstall itself and remove its local files. If it is offline, cleanup will run the next time it connects.`,
    );
    if (!confirmed) return;
    setDeletingId(agent.id);
    reportActionError("delete", null);
    try {
      const response = await authorizedFetch(`/v1/agents/${encodeURIComponent(agent.id)}`, { method: "DELETE" });
      if (!response.ok) throw new Error(await errorMessage(response, "The device could not be deleted."));
      // The server publishes agent_deleted to the event socket.
    } catch (requestError) {
      if (!(requestError instanceof AuthenticationRequired)) {
        reportActionError("delete", requestError instanceof Error ? requestError.message : "The device could not be deleted.");
      }
    } finally {
      setDeletingId(null);
    }
  };

  if (enrolling && view !== "account") return <Navigate to={VIEW_PATHS.account} replace />;
  if (!canView(account, view) && view === "devices") {
    const home = homeView(account);
    if (home !== "devices") return <Navigate to={VIEW_PATHS[home]} replace />;
  }

  const instanceName = instance?.name ?? "MeshRMM";
  const workspace: Workspace = {
    account,
    instanceName,
    can,
    refreshAccount,
    authorizedFetch,
    inventory,
    remote,
    deleteAgent,
    deletingId,
    actionErrors,
    reportActionError,
    devicesSearch,
    setDevicesSearch,
    settings,
    setSettings,
    settingsDraft,
  };

  const pill = inventoryStatus === "live" ? "Connected"
    : inventoryStatus === "stale" ? CONNECTION_PILL[connection]
    : "Signed in";
  const navigation = (
    <nav aria-label="Primary navigation">
      {NAV_GROUPS.map(({ label, views }) => {
        const visible = views.filter((candidate) => canView(account, candidate));
        if (!visible.length) return null;
        return (
          <div key={label} className="nav-group">
            <p className="nav-label">{label}</p>
            {visible.map((candidate) => {
              const Icon = NAV_ICONS[candidate];
              return (
                <NavItem key={candidate} view={candidate} current={view} disabled={enrolling} search={candidate === "devices" ? devicesSearch : ""}>
                  <Icon size={18} /><span>{VIEW_COPY[candidate].title}</span>
                  {candidate === "devices" && hasData ? <em>{agents.length}</em> : null}
                </NavItem>
              );
            })}
          </div>
        );
      })}
    </nav>
  );

  return (
    <WorkspaceContext.Provider value={workspace}>
      <Frame
        instanceName={instanceName}
        pill={pill}
        pillState={inventoryStatus === "stale" ? "stale" : inventoryStatus === "live" ? "live" : ""}
        navigation={navigation}
        viewerCard={can("sessions.connect")}
        profile={<ProfileRow account={account} active={view === "account"} onSignOut={() => { resetInventory(); void signOut(); }} />}
        sidebarOpen={isSidebarOpen}
        onSidebar={setIsSidebarOpen}
      >
        <PageHeading view={view}>
          {view === "devices" && canView(account, "devices") && <div className="heading-actions">
            <button className="secondary-button" onClick={() => void refreshInventory()}><RefreshCw size={16} className={isRefreshing ? "spin" : ""} /> Refresh</button>
            {can("devices.enroll") && <button className="primary-button" onClick={(event) => { dialogOpener.current = event.currentTarget; installer.reset(); setIsAgentOpen(true); }} aria-haspopup="dialog"><Plus size={16} /> Add device</button>}
          </div>}
        </PageHeading>
        {canView(account, view) ? <Outlet /> : <NoAccessPanel />}
      </Frame>

      {isAgentOpen && (
        <EnrollmentModal
          instanceName={instanceName}
          platform={installer.platform}
          error={installer.error}
          isDownloading={installer.isDownloading}
          downloaded={installer.downloaded}
          command={installer.command}
          onClose={() => setIsAgentOpen(false)}
          onPlatformChange={installer.setPlatform}
          onSubmit={(event) => void installer.download(event)}
          returnFocus={dialogOpener}
        />
      )}
    </WorkspaceContext.Provider>
  );
}

function Frame({ instanceName, pill, pillState = "", navigation, viewerCard = false, profile, sidebarOpen = false, onSidebar, children }: {
  instanceName: string | undefined;
  pill: string;
  pillState?: "" | "live" | "stale";
  navigation?: ReactNode;
  viewerCard?: boolean;
  profile?: ReactNode;
  sidebarOpen?: boolean;
  onSidebar?: (open: boolean) => void;
  children: ReactNode;
}) {
  return (
    <div className="app-shell">
      <aside className={`sidebar ${sidebarOpen ? "sidebar-open" : ""}`}>
        <div className="brand-row">
          <div className="brand-mark"><Network size={19} strokeWidth={2.5} /></div>
          <span>Mesh<span>RMM</span></span>
          <button className="sidebar-close" onClick={() => onSidebar?.(false)} aria-label="Close navigation"><X size={20} /></button>
        </div>

        <div className="workspace-identity">
          <div className="workspace-avatar">{instanceName ? initials(instanceName) : "··"}</div>
          <div><strong>{instanceName ?? "MeshRMM"}</strong><span>Self-hosted server</span></div>
        </div>

        {navigation}
        {viewerCard && <ViewerDownloadCard />}
        {profile}
      </aside>

      {sidebarOpen && <button className="sidebar-scrim" onClick={() => onSidebar?.(false)} aria-label="Close navigation" />}

      <main className="main-content">
        <header className="topbar">
          <button className="mobile-menu" onClick={() => onSidebar?.(true)} aria-label="Open navigation" aria-expanded={sidebarOpen} disabled={!onSidebar}><Menu size={21} /></button>
          <div className="workspace-breadcrumb"><Building2 size={16} /><span>{instanceName ?? "MeshRMM"}</span></div>
          <div className="topbar-actions">
            <div className="connection-pill" role="status">
              <span className={`status-dot ${pillState}`} />
              {pill}
            </div>
          </div>
        </header>
        <div className="page-wrap">{children}</div>
      </main>
    </div>
  );
}

function PageHeading({ view, children }: { view: View; children?: ReactNode }) {
  return (
    <section className="page-heading">
      <div>
        <h1>{VIEW_COPY[view].title}</h1>
        <p>{VIEW_COPY[view].description}</p>
      </div>
      {children}
    </section>
  );
}

function LoadingPanel() {
  return <section className="management-panel account-status"><p role="status"><LoaderCircle size={16} className="spin" /> Loading…</p></section>;
}

function UnavailablePanel({ message, retrying }: { message: string; retrying: boolean }) {
  const { retry } = useSession();
  return (
    <section className="management-panel account-status">
      <h2>MeshRMM could not be loaded</h2>
      <p role="alert">{message}</p>
      <p>{retrying ? "MeshRMM will keep retrying." : "Resolve the problem, then try again."}</p>
      <button className="secondary-button" onClick={retry}><RefreshCw size={16} /> Retry now</button>
    </section>
  );
}

function NoAccessPanel() {
  return (
    <section className="management-panel account-status">
      <h2><ShieldAlert size={16} aria-hidden="true" /> You don&apos;t have access to this page</h2>
      <p>Your roles don&apos;t include it. Ask an administrator if you need it.</p>
    </section>
  );
}

const PAUSED_COPY: Record<LockReason, { title: string; detail: (minutes: number | null) => string }> = {
  idle: {
    title: "You’ve been signed out for inactivity",
    detail: (minutes) => `This server signs out inactive browsers after ${minutes === null ? "a while" : formatIdleTimeout(minutes)}.`,
  },
  expired: { title: "Your session has ended", detail: () => "Sign in again to continue." },
  elsewhere: { title: "You signed out in another tab", detail: () => "Sign in again to continue here." },
};

function PausedCard({ reason, idleTimeoutMinutes }: { reason: LockReason; idleTimeoutMinutes: number | null }) {
  const location = useLocation();
  const copy = PAUSED_COPY[reason];
  return (
    <section className="signed-out-card session-paused-card">
      <div className="modal-icon"><Clock3 size={22} /></div>
      <p className="eyebrow">Session paused</p>
      <h1>{copy.title}</h1>
      <p>{copy.detail(idleTimeoutMinutes)}</p>
      <Link className="primary-button" to={loginPath(`${location.pathname}${location.search}`)}><LogIn size={16} /> Sign in again</Link>
    </section>
  );
}

function ProfileRow({ account, active, onSignOut }: { account: Account; active: boolean; onSignOut: () => void }) {
  return (
    <div className="profile-row">
      <Link to={VIEW_PATHS.account} className={`profile-link${active ? " active" : ""}`} aria-current={active ? "page" : undefined}>
        <div className="profile-avatar">{initials(account.user.display_name || account.user.email)}</div>
        <div className="profile-details"><strong>{account.user.display_name}</strong><span>{account.user.email}</span></div>
      </Link>
      <button type="button" className="profile-sign-out" onClick={onSignOut} aria-label="Sign out" title="Sign out"><LogOut size={16} /></button>
    </div>
  );
}

function initials(name: string) {
  const words = name.trim().split(/\s+/u).filter(Boolean);
  const letters = words.length > 1 ? [words[0], words[words.length - 1]] : [words[0] ?? ""];
  return letters.map((word) => Array.from(word)[0] ?? "").join("").toUpperCase() || "··";
}

// Sidebar entries are links so they can open in a new tab. Unavailable
// entries stay disabled buttons.
function NavItem({ view, search = "", current, disabled, children }: {
  view: View;
  // Carries the Devices filters so returning to Devices restores them.
  search?: string;
  current: View;
  disabled: boolean;
  children: ReactNode;
}) {
  const className = `nav-item ${view === current ? "active" : ""}`;
  const ariaCurrent = view === current ? "page" : undefined;
  if (disabled) {
    return <button type="button" className={className} aria-current={ariaCurrent} disabled>{children}</button>;
  }
  return <Link to={`${VIEW_PATHS[view]}${search}`} className={className} aria-current={ariaCurrent}>{children}</Link>;
}
