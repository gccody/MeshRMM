// The workspace's pages. The shell derives the current one from the
// pathname, since it stays mounted while the pages beneath it change.
// Imports carry the .ts extension so node:test can load this module directly.
import { type Account, type Permission, canAny } from "../auth/types.ts";

export type View = "devices" | "device" | "companies" | "software" | "toolbox" | "alerts" | "alert-rules" | "notification-channels" | "users" | "roles" | "authentication" | "settings" | "audit" | "account" | "jobs" | "job" | "policies";

export const VIEW_PATHS: Record<View, string> = {
  devices: "/",
  device: "/device",
  companies: "/companies",
  software: "/software",
  toolbox: "/toolbox",
  alerts: "/alerts",
  "alert-rules": "/alert-rules",
  "notification-channels": "/notification-channels",
  users: "/users",
  roles: "/roles",
  authentication: "/authentication",
  settings: "/settings",
  audit: "/audit",
  // After the audit log, so its readers (who see jobs too) still land on it.
  jobs: "/jobs",
  job: "/job",
  policies: "/policies",
  account: "/account",
};

// Each page's title, which its navigation entry shows too.
export const VIEW_COPY: Record<View, { title: string }> = {
  devices: { title: "Devices" },
  device: { title: "Device" },
  companies: { title: "Companies" },
  software: { title: "Software" },
  toolbox: { title: "Toolbox" },
  alerts: { title: "Alerts" },
  "alert-rules": { title: "Alert rules" },
  "notification-channels": { title: "Notification channels" },
  users: { title: "Users" },
  roles: { title: "Roles" },
  authentication: { title: "Authentication" },
  settings: { title: "Settings" },
  audit: { title: "Audit log" },
  account: { title: "Your account" },
  jobs: { title: "Jobs" },
  job: { title: "Job" },
  policies: { title: "Policies" },
};

// Jobs show to those who start them, policies' managers and auditors.
const JOB_ACCESS: readonly Permission[] = ["scripts.run", "files.deliver", "automation.manage", "audit.view"];

// A user sees a page when they hold any of these permissions; an empty list
// is open to everyone.
const VIEW_ACCESS: Record<View, readonly Permission[]> = {
  devices: ["devices.view"],
  device: ["devices.view"],
  companies: ["devices.view"],
  software: ["devices.view"],
  toolbox: ["scripts.run", "scripts.manage_shared", "files.deliver", "files.manage_shared"],
  alerts: ["alerts.view"],
  "alert-rules": ["alerts.view"],
  "notification-channels": ["alerts.manage"],
  users: ["users.manage"],
  roles: ["roles.manage"],
  authentication: ["authentication.manage"],
  settings: ["settings.manage"],
  audit: ["audit.view"],
  account: [],
  jobs: JOB_ACCESS,
  job: JOB_ACCESS,
  policies: ["automation.manage"],
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

// The navigation entry a page sits under: a device under Devices, the alert
// rules and channels under Alerts, and a job and the policies under Jobs.
export function navViewFor(view: View): View {
  if (view === "device") return "devices";
  if (view === "job" || view === "policies") return "jobs";
  if (view === "alert-rules" || view === "notification-channels") return "alerts";
  return view;
}

export function viewForPath(pathname: string | null | undefined): View | null {
  const path = (pathname ?? "/").replace(/\/+$/, "") || "/";
  for (const [view, viewPath] of Object.entries(VIEW_PATHS) as [View, string][]) {
    if (viewPath === path) return view;
  }
  return null;
}
