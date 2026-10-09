// Every page the server serves, with the title its prerendered HTML carries
// before the browser knows the server's name. The build writes each to
// `<path>/index.html` (the root to `index.html`), and an unknown path gets
// `404.html`.
export const PAGES: readonly { path: string; title: string }[] = [
  { path: "/", title: "Devices" },
  { path: "/device", title: "Device" },
  { path: "/toolbox", title: "Toolbox" },
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
