// The workspace's pages. The shell derives the current one from the
// pathname, since it stays mounted while the pages beneath it change.
// Imports carry the .ts extension so node:test can load this module directly.
import { type Account, type Permission, canAny } from "../auth/types.ts";

export type View = "devices" | "toolbox" | "users" | "roles" | "authentication" | "settings" | "audit" | "account";

export const VIEW_PATHS: Record<View, string> = {
  devices: "/",
  toolbox: "/toolbox",
  users: "/users",
  roles: "/roles",
  authentication: "/authentication",
  settings: "/settings",
  audit: "/audit",
  account: "/account",
};

export const VIEW_COPY: Record<View, { title: string; description: string }> = {
  devices: { title: "Devices", description: "Connect to your devices and keep your team working." },
  toolbox: { title: "Toolbox", description: "Scripts and files to run on or send to your devices." },
  users: { title: "Users", description: "Invite your team and manage their access." },
  roles: { title: "Roles", description: "Choose what each role lets its members do." },
  authentication: { title: "Authentication", description: "Set the sign-in policy, single sign-on, directory sync and how MeshRMM sends email." },
  settings: { title: "Settings", description: "Name this server and set remote session defaults." },
  audit: { title: "Audit log", description: "Who signed in and what they changed." },
  account: { title: "Your account", description: "Your profile, password, two-factor authentication, passkeys and sessions." },
};

// A user sees a page when they hold any of these permissions; an empty list
// is open to everyone.
const VIEW_ACCESS: Record<View, readonly Permission[]> = {
  devices: ["devices.view"],
  toolbox: ["scripts.run", "scripts.manage_shared", "files.deliver", "files.manage_shared"],
  users: ["users.manage"],
  roles: ["roles.manage"],
  authentication: ["authentication.manage"],
  settings: ["settings.manage"],
  audit: ["audit.view"],
  account: [],
};

export function canView(account: Pick<Account, "permissions" | "is_administrator"> | null, view: View) {
  if (!account) return false;
  const required = VIEW_ACCESS[view];
  // Only administrators manage single sign-on, directory sync and email, on
  // the Authentication page.
  if (view === "authentication" && account.is_administrator) return true;
  return required.length === 0 || canAny(account, required);
}

// Where a user lands: Devices if they may see it, else their first page.
export function homeView(account: Pick<Account, "permissions" | "is_administrator"> | null): View {
  const views = Object.keys(VIEW_PATHS) as View[];
  return views.find((view) => canView(account, view)) ?? "account";
}

export function viewForPath(pathname: string | null | undefined): View | null {
  const path = (pathname ?? "/").replace(/\/+$/, "") || "/";
  for (const [view, viewPath] of Object.entries(VIEW_PATHS) as [View, string][]) {
    if (viewPath === path) return view;
  }
  return null;
}
