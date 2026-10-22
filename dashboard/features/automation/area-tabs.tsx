import { CalendarClock, ListChecks } from "lucide-react";
import { Link } from "react-router";
import { VIEW_PATHS, type View, canView } from "../workspace/views";
import { useWorkspace } from "../workspace/workspace-context";

const TABS: { view: View; label: string; icon: typeof ListChecks }[] = [
  { view: "jobs", label: "Jobs", icon: ListChecks },
  { view: "policies", label: "Policies", icon: CalendarClock },
];

// The Jobs area's pages, as links above each of them.
export function AutomationAreaTabs({ current }: { current: View }) {
  const { account } = useWorkspace();
  const tabs = TABS.filter(({ view }) => canView(account, view));
  if (tabs.length < 2) return null;
  return (
    <nav className="toolbox-tabs area-tabs" aria-label="Jobs">
      {tabs.map(({ view, label, icon: Icon }) => (
        <Link key={view} to={VIEW_PATHS[view]} aria-current={current === view ? "page" : undefined}>
          <Icon size={16} aria-hidden="true" />{label}
        </Link>
      ))}
    </nav>
  );
}
