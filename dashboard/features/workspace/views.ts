// The workspace's pages. The shell derives the current one from the
// pathname, since it stays mounted while the pages beneath it change.
// Imports carry the .ts extension so node:test can load this module directly.
import { type Account, type Permission, canAny } from "../auth/types.ts";

export type View = "devices" | "device" | "toolbox" | "users" | "roles" | "authentication" | "settings" | "audit" | "account";

export const VIEW_PATHS: Record<View, string> = {
  devices: "/",
  device: "/device",
  toolbox: "/toolbox",
  users: "/users",
  roles: "/roles",
  authentication: "/authentication",
  settings: "/settings",
  audit: "/audit",
  account: "/account",
};

// Each page's title, which its navigation entry shows too.
export const VIEW_COPY: Record<View, { title: string }> = {
  devices: { title: "Devices" },
  device: { title: "Device" },
  toolbox: { title: "Toolbox" },
  users: { title: "Users" },
  roles: { title: "Roles" },
  authentication: { title: "Authentication" },
  settings: { title: "Settings" },
  audit: { title: "Audit log" },
  account: { title: "Your account" },
};

// A user sees a page when they hold any of these permissions; an empty list
// is open to everyone.
const VIEW_ACCESS: Record<View, readonly Permission[]> = {
  devices: ["devices.view"],
  device: ["devices.view"],
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
