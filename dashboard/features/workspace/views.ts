// The company workspace's pages. The shell derives the current one from the
// pathname, since it stays mounted while the pages beneath it change.
export type View = "agents" | "team" | "sso" | "settings";

export const VIEW_PATHS: Record<View, string> = {
  agents: "/",
  team: "/users",
  sso: "/authentication",
  settings: "/settings",
};

export const VIEW_COPY: Record<View, { title: string; description: string }> = {
  agents: { title: "Devices", description: "Connect to your devices and keep your team working." },
  team: { title: "Users", description: "Invite your team and manage their access." },
  sso: { title: "Authentication", description: "Manage company domains and single sign-on." },
  settings: { title: "Settings", description: "Manage company security and remote session defaults." },
};

export function viewForPath(pathname: string | null | undefined): View {
  const path = (pathname ?? "/").replace(/\/+$/, "") || "/";
  for (const [view, viewPath] of Object.entries(VIEW_PATHS) as [View, string][]) {
    if (viewPath === path) return view;
  }
  return "agents";
}
