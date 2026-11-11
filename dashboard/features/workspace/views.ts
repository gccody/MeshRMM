// The workspace's pages. The shell derives the current one from the
// pathname, since it stays mounted while the pages beneath it change.
// Imports carry the .ts extension so node:test can load this module directly.
import { type Account, type Permission, canAny } from "../auth/types.ts";

export type View = "devices" | "device" | "companies" | "company" | "custom-fields" | "tags" | "maintenance" | "deployment" | "software" | "updates" | "applications" | "toolbox" | "alerts" | "alert-rules" | "reports" | "notification-channels" | "users" | "roles" | "authentication" | "settings" | "audit" | "account" | "jobs" | "job" | "policies" | "terminal" | "files";

export const VIEW_PATHS: Record<View, string> = {
  devices: "/",
  device: "/device",
  companies: "/companies",
  company: "/company",
  "custom-fields": "/custom-fields",
  tags: "/tags",
  maintenance: "/maintenance",
  deployment: "/deployment",
  software: "/software",
  updates: "/updates",
  applications: "/applications",
  toolbox: "/toolbox",
  alerts: "/alerts",
  "alert-rules": "/alert-rules",
  "notification-channels": "/notification-channels",
  reports: "/reports",
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
  // Last, so nobody lands on it: it needs a device.
  terminal: "/terminal",
  files: "/files",
};

// Each page's title, which its navigation entry shows too.
export const VIEW_COPY: Record<View, { title: string }> = {
  devices: { title: "Devices" },
  device: { title: "Device" },
  companies: { title: "Companies" },
  company: { title: "Company" },
  "custom-fields": { title: "Custom fields" },
  tags: { title: "Tags" },
  maintenance: { title: "Maintenance" },
  deployment: { title: "Deployment tokens" },
  software: { title: "Software" },
  updates: { title: "Updates" },
  applications: { title: "Applications" },
  toolbox: { title: "Toolbox" },
  alerts: { title: "Alerts" },
  "alert-rules": { title: "Alert rules" },
  "notification-channels": { title: "Notification channels" },
  reports: { title: "Reports" },
  users: { title: "Users" },
  roles: { title: "Roles" },
  authentication: { title: "Authentication" },
  settings: { title: "Settings" },
  audit: { title: "Audit log" },
  account: { title: "Your account" },
  jobs: { title: "Jobs" },
  job: { title: "Job" },
  policies: { title: "Policies" },
  terminal: { title: "Terminal" },
  files: { title: "Files" },
};

// Jobs show to those who start them, policies' managers and auditors.
const JOB_ACCESS: readonly Permission[] = ["scripts.run", "files.deliver", "updates.view", "updates.install", "applications.view", "applications.deploy", "automation.manage", "audit.view"];

// A user sees a page when they hold any of these permissions; an empty list
// is open to everyone.
const VIEW_ACCESS: Record<View, readonly Permission[]> = {
  devices: ["devices.view"],
  device: ["devices.view"],
  companies: ["devices.view"],
  company: ["devices.view"],
  "custom-fields": ["custom_fields.manage"],
  tags: ["devices.view"],
  maintenance: ["devices.view"],
  deployment: ["devices.enroll"],
  software: ["devices.view"],
  updates: ["updates.view"],
  applications: ["applications.view"],
  toolbox: ["scripts.run", "scripts.manage_shared", "files.deliver", "files.manage_shared"],
  alerts: ["alerts.view"],
  "alert-rules": ["alerts.view"],
  "notification-channels": ["alerts.manage"],
  reports: ["reports.view"],
  users: ["users.manage"],
  roles: ["roles.manage"],
  authentication: ["authentication.manage", "api_tokens.manage"],
  settings: ["settings.manage"],
  audit: ["audit.view"],
  account: [],
  jobs: JOB_ACCESS,
  job: JOB_ACCESS,
  policies: ["automation.manage"],
  terminal: ["terminal.connect", "terminal.connect_system"],
  files: ["files.browse"],
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

// The navigation entry a page sits under: a device, its terminals and files under
// Devices, a company, custom fields, tags, maintenance and deployment tokens under Companies, the alert rules and channels under Alerts,
// and a job and the policies under Jobs.
export function navViewFor(view: View): View {
  if (view === "device" || view === "terminal" || view === "files") return "devices";
  if (view === "company" || view === "custom-fields" || view === "tags" || view === "maintenance" || view === "deployment") return "companies";
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
