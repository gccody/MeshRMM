// Every page the server serves, with the title its prerendered HTML carries
// before the browser knows the server's name. The build writes each to
// `<path>/index.html` (the root to `index.html`), and an unknown path gets
// `404.html`.
export const PAGES: readonly { path: string; title: string }[] = [
  { path: "/", title: "Devices" },
  { path: "/device", title: "Device" },
  { path: "/terminal", title: "Terminal" },
  { path: "/files", title: "Files" },
  { path: "/companies", title: "Companies" },
  { path: "/tags", title: "Tags" },
  { path: "/software", title: "Software" },
  { path: "/updates", title: "Updates" },
  { path: "/toolbox", title: "Toolbox" },
  { path: "/jobs", title: "Jobs" },
  { path: "/job", title: "Job" },
  { path: "/policies", title: "Policies" },
  { path: "/alerts", title: "Alerts" },
  { path: "/alert-rules", title: "Alert rules" },
  { path: "/notification-channels", title: "Notification channels" },
  { path: "/users", title: "Users" },
  { path: "/roles", title: "Roles" },
  { path: "/authentication", title: "Authentication" },
  { path: "/settings", title: "Settings" },
  { path: "/audit", title: "Audit log" },
  { path: "/account", title: "Your account" },
  { path: "/login", title: "Sign in" },
  { path: "/setup", title: "First-run setup" },
  { path: "/invite", title: "Accept invitation" },
  { path: "/reset", title: "Reset password" },
];

export const NOT_FOUND_TITLE = "Page not found";

// A path no page has, rendered as 404.html.
export const NOT_FOUND_PATH = "/404";
