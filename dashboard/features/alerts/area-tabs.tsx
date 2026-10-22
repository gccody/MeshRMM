import { Bell, ListChecks, Send } from "lucide-react";
import { Link } from "react-router";
import { VIEW_PATHS, type View } from "../workspace/views";
import { useWorkspace } from "../workspace/workspace-context";

const TABS: { view: View; label: string; icon: typeof Bell }[] = [
  { view: "alerts", label: "Alerts", icon: Bell },
  { view: "alert-rules", label: "Rules", icon: ListChecks },
  { view: "notification-channels", label: "Notification channels", icon: Send },
];

// The Alerts area's pages, as links above each of them.
export function AlertAreaTabs({ current }: { current: View }) {
  const { can } = useWorkspace();
  const tabs = TABS.filter(({ view }) => view !== "notification-channels" || can("alerts.manage"));
  return (
    <nav className="toolbox-tabs area-tabs" aria-label="Alerts">
      {tabs.map(({ view, label, icon: Icon }) => (
        <Link key={view} to={VIEW_PATHS[view]} aria-current={current === view ? "page" : undefined}>
          <Icon size={16} aria-hidden="true" />{label}
        </Link>
      ))}
    </nav>
  );
}
