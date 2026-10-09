import { LoaderCircle, LogIn, LogOut, Menu, MonitorDown, RefreshCw, UserRound, X } from "lucide-react";
import { type ReactNode, useCallback, useReducer, useState } from "react";
import { Link, Navigate, Outlet, useLocation } from "react-router";
import { AuthenticationRequired, apiFetch, errorMessage } from "../../lib/http";
import { usePopover } from "../../lib/use-popover";
import { useAgentInventory } from "../agents/use-agent-inventory";
import type { InventoryConnection } from "../agents/inventory-stream";
import type { Agent } from "../agents/types";
import { loginPath } from "../auth/next-path";
import { type LockReason, useDocumentTitle, useSession } from "../auth/session";
import type { Account, Permission } from "../auth/types";
import { formatIdleTimeout } from "../session/idle-session";
import { useIdleSession } from "../session/use-idle-session";
import { useRemoteHandoff } from "../session/use-remote-handoff";
import { useViewerPlatform } from "../session/use-viewer-platform";
import { ViewerDownloadLinks } from "../session/viewer-download-links";
import type { GeneralSettings } from "../settings/general-settings";
import { useSettingsDraft } from "../settings/use-settings-draft";
import { type ActionErrorSource, actionErrorsReducer } from "./action-errors";
import { VIEW_COPY, VIEW_PATHS, type View, canView, homeView, viewForPath } from "./views";
import { type Workspace, WorkspaceContext } from "./workspace-context";

// The topbar status while the inventory on screen is out of date.
const CONNECTION_STATUS: Record<InventoryConnection, string> = {
  connecting: "Reconnecting",
  live: "Live",
  reconnecting: "Reconnecting",
  offline: "Offline",
};

// Day-to-day pages, then administration, which the bar sets apart.
const NAV_GROUPS: View[][] = [
  ["devices", "toolbox"],
  ["users", "roles", "authentication", "settings", "audit"],
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
  // A page such as a device's names itself.
  const [pageTitle, setPageTitle] = useState<string | null>(null);
  const title = pageTitle ?? VIEW_COPY[view].title;
  useDocumentTitle(title);

  if (instance?.setup_required) return <Navigate to="/setup" replace />;
  if (state.status === "signed-out" && state.reason === null) {
    return <Navigate to={loginPath(`${location.pathname}${location.search}`)} replace />;
  }
  if (state.status === "signed-in") return <SignedInWorkspace key={state.account.user.id} account={state.account} view={view} title={title} setPageTitle={setPageTitle} />;

  const status = state.status === "loading" ? "Checking session" : state.status === "unavailable" ? "Unavailable" : "Session paused";
  return (
    <Frame instanceName={instance?.name} status={status}>
      {state.status === "signed-out" && state.reason !== null
        ? <PausedCard reason={state.reason} idleTimeoutMinutes={state.idleTimeoutMinutes ?? null} />
        : (
          <>
            <PageHeading title={title} />
            {state.status === "unavailable" ? <UnavailablePanel message={state.message} retrying={state.retrying} /> : <LoadingPanel />}
          </>
        )}
    </Frame>
  );
}

function SignedInWorkspace({ account, view, title, setPageTitle }: {
  account: Account;
  view: View;
  title: string;
  setPageTitle: (title: string | null) => void;
}) {
  const { instance, refresh, signOut, lock } = useSession();
  const location = useLocation();
  const [devicesSearch, setDevicesSearch] = useState("");
  const [isNavOpen, setIsNavOpen] = useState(false);
  const [headerSlot, setHeaderSlot] = useState<HTMLElement | null>(null);
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
  const { agents, hasData, status: inventoryStatus, connection, reset: resetInventory } = inventory;
  const reportActionError = useCallback(
    (source: ActionErrorSource, message: string | null) => dispatchActionError({ source, message }),
    [],
  );
  const reportRemoteError = useCallback((message: string | null) => reportActionError("remote", message), [reportActionError]);
  const remote = useRemoteHandoff({ authorizedFetch, reportError: reportRemoteError });
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

  // Navigating closes the small-screen menu, so it doesn't cover the new page.
  const [navPath, setNavPath] = useState(location.pathname);
  if (navPath !== location.pathname) {
    setNavPath(location.pathname);
    setIsNavOpen(false);
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
    headerSlot,
    setPageTitle,
  };

  const status = inventoryStatus === "live" ? "Live"
    : inventoryStatus === "stale" ? CONNECTION_STATUS[connection]
    : "Signed in";
  const navigation = (
    <nav className={`primary-nav${isNavOpen ? " nav-open" : ""}`} id="primary-nav" aria-label="Primary navigation">
      {NAV_GROUPS.map((group) => {
        const visible = group.filter((candidate) => canView(account, candidate));
        if (!visible.length) return null;
        return (
          <div key={group[0]} className="nav-group">
            {visible.map((candidate) => (
              <NavItem key={candidate} view={candidate} current={view === "device" ? "devices" : view} disabled={enrolling} search={candidate === "devices" ? devicesSearch : ""}>
                {VIEW_COPY[candidate].title}
                {candidate === "devices" && hasData ? <em>{agents.length}</em> : null}
              </NavItem>
            ))}
          </div>
        );
      })}
    </nav>
  );

  return (
    <WorkspaceContext.Provider value={workspace}>
      <Frame
        instanceName={instanceName}
        status={status}
        statusState={inventoryStatus === "stale" ? "stale" : inventoryStatus === "live" ? "live" : ""}
        navigation={navigation}
        navOpen={isNavOpen}
        onNav={setIsNavOpen}
        tools={<>
          {can("sessions.connect") && <RemoteAppMenu />}
          <AccountMenu account={account} active={view === "account"} onSignOut={() => { resetInventory(); void signOut(); }} />
        </>}
      >
        <PageHeading title={title} slot={setHeaderSlot} />
        {canView(account, view) ? <Outlet /> : <NoAccessPanel />}
      </Frame>
    </WorkspaceContext.Provider>
  );
}

function Frame({ instanceName, status, statusState = "", navigation, navOpen = false, onNav, tools, children }: {
  instanceName: string | undefined;
  status: string;
  statusState?: "" | "live" | "stale";
  navigation?: ReactNode;
  navOpen?: boolean;
  onNav?: (open: boolean) => void;
  tools?: ReactNode;
  children: ReactNode;
}) {
  return (
    <div className="app-shell">
      <header className="topbar">
        {navigation && (
          <button className="nav-toggle" onClick={() => onNav?.(!navOpen)} aria-label={navOpen ? "Close navigation" : "Open navigation"} aria-expanded={navOpen} aria-controls="primary-nav">
            {navOpen ? <X size={20} /> : <Menu size={20} />}
          </button>
        )}
        <Link to="/" className="brand" aria-label={`${instanceName ?? "MeshRMM"} home`}>
          <BrandMark />
          <span>{instanceName ?? "MeshRMM"}</span>
        </Link>
        {navigation}
        <div className="topbar-tools">
          <span className={`connection-status ${statusState}`} role="status"><i aria-hidden="true" />{status}</span>
          {tools}
        </div>
      </header>
      <main className="page-wrap">{children}</main>
    </div>
  );
}

// Three linked nodes: a mesh.
export function BrandMark() {
  return (
    <svg className="brand-mark" viewBox="0 0 24 24" width="24" height="24" aria-hidden="true">
      <path d="M6 17.5 12 6l6 11.5H6Z" fill="none" stroke="currentColor" strokeWidth="1.6" strokeLinejoin="round" />
      <circle cx="12" cy="6" r="3" fill="currentColor" />
      <circle cx="6" cy="17.5" r="3" fill="currentColor" />
      <circle cx="18" cy="17.5" r="3" fill="currentColor" />
    </svg>
  );
}

function PageHeading({ title, slot }: { title: string; slot?: (element: HTMLElement | null) => void }) {
  return (
    <section className="page-heading">
      <h1>{title}</h1>
      {slot && <div className="heading-actions" ref={slot} />}
    </section>
  );
}

// The viewer is needed to connect, so its download stays one click away.
function RemoteAppMenu() {
  const platform = useViewerPlatform();
  const { open, setOpen, container, trigger } = usePopover();
  return (
    <div className="popover-anchor" ref={container}>
      <button ref={trigger} type="button" className="tool-button" onClick={() => setOpen(!open)} aria-expanded={open} aria-controls={open ? "remote-app-panel" : undefined} title="Get MeshRMM Remote">
        <MonitorDown size={17} aria-hidden="true" /><span className="tool-label">Remote app</span>
      </button>
      {open && (
        <section id="remote-app-panel" className="popover remote-app-panel" aria-labelledby="remote-app-title">
          <h2 id="remote-app-title">MeshRMM Remote</h2>
          <p>Install it on this computer to connect to devices.</p>
          <ViewerDownloadLinks platform={platform} />
        </section>
      )}
    </div>
  );
}

function AccountMenu({ account, active, onSignOut }: { account: Account; active: boolean; onSignOut: () => void }) {
  const { open, setOpen, container, trigger } = usePopover();
  return (
    <div className="popover-anchor" ref={container}>
      <button ref={trigger} type="button" className={`avatar-button${active ? " active" : ""}`} onClick={() => setOpen(!open)} aria-expanded={open} aria-controls={open ? "account-menu" : undefined} aria-label={`Account: ${account.user.display_name}`}>
        {initials(account.user.display_name || account.user.email)}
      </button>
      {open && (
        <div id="account-menu" className="popover menu account-menu">
          <div className="account-menu-identity"><strong>{account.user.display_name}</strong><span>{account.user.email}</span></div>
          <Link to={VIEW_PATHS.account} onClick={() => setOpen(false)} aria-current={active ? "page" : undefined}><UserRound size={16} aria-hidden="true" />Your account</Link>
          <button type="button" onClick={onSignOut}><LogOut size={16} aria-hidden="true" />Sign out</button>
        </div>
      )}
    </div>
  );
}

function LoadingPanel() {
  return <p className="page-status" role="status"><LoaderCircle size={16} className="spin" /> Loading…</p>;
}

function UnavailablePanel({ message, retrying }: { message: string; retrying: boolean }) {
  const { retry } = useSession();
  return (
    <section className="notice-card">
      <h2>MeshRMM could not be loaded</h2>
      <p role="alert">{message}</p>
      <p>{retrying ? "Retrying automatically." : "Resolve the problem, then try again."}</p>
      <button className="secondary-button" onClick={retry}><RefreshCw size={16} /> Retry now</button>
    </section>
  );
}

function NoAccessPanel() {
  return (
    <section className="notice-card">
      <h2>You don&apos;t have access to this page</h2>
      <p>Ask an administrator if you need it.</p>
    </section>
  );
}

const PAUSED_COPY: Record<LockReason, { title: string; detail: (minutes: number | null) => string }> = {
  idle: {
    title: "Signed out for inactivity",
    detail: (minutes) => `This server signs out inactive browsers after ${minutes === null ? "a while" : formatIdleTimeout(minutes)}.`,
  },
  expired: { title: "Your session has ended", detail: () => "Sign in again to continue." },
  elsewhere: { title: "You signed out in another tab", detail: () => "Sign in again to continue here." },
};

function PausedCard({ reason, idleTimeoutMinutes }: { reason: LockReason; idleTimeoutMinutes: number | null }) {
  const location = useLocation();
  const copy = PAUSED_COPY[reason];
  return (
    <section className="notice-card session-paused-card">
      <h1>{copy.title}</h1>
      <p>{copy.detail(idleTimeoutMinutes)}</p>
      <Link className="primary-button" to={loginPath(`${location.pathname}${location.search}`)}><LogIn size={16} /> Sign in again</Link>
    </section>
  );
}

function initials(name: string) {
  const words = name.trim().split(/\s+/u).filter(Boolean);
  const letters = words.length > 1 ? [words[0], words[words.length - 1]] : [words[0] ?? ""];
  return letters.map((word) => Array.from(word)[0] ?? "").join("").toUpperCase() || "··";
}

// Navigation entries are links so they can open in a new tab. Unavailable
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
