import { MarketingPage } from "../../features/marketing/marketing-page";
import { PlatformDashboard } from "../../features/platform/platform-dashboard";
import { WorkspaceShell } from "../../features/workspace/workspace-shell";
import { requestSurface } from "../../lib/request-surface";

// The company workspace's pages share this layout. Route groups don't change
// the URL or the layout's identity, so the shell stays mounted across them and
// only the page's panel changes. The marketing site and the owner console take
// the same paths but ignore the page.
export default async function WorkspaceLayout({ children }: Readonly<{ children: React.ReactNode }>) {
  const surface = await requestSurface();
  if (surface === "marketing") return <MarketingPage />;
  if (surface === "platform") return <PlatformDashboard />;
  return <WorkspaceShell>{children}</WorkspaceShell>;
}
