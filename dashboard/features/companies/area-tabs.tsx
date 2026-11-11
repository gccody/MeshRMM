import { Building2, KeyRound, Tags, Wrench } from "lucide-react";
import { Link } from "react-router";
import { useWorkspace } from "../workspace/workspace-context";
import { VIEW_PATHS, type View, canView } from "../workspace/views";

const TABS: { view: View; label: string; icon: typeof Building2 }[] = [
  { view: "companies", label: "Companies", icon: Building2 },
  { view: "tags", label: "Tags", icon: Tags },
  { view: "maintenance", label: "Maintenance", icon: Wrench },
  { view: "deployment", label: "Deployment tokens", icon: KeyRound },
];

// The ways of grouping devices, and the tokens that enroll many into a
// company, as links above each page.
export function GroupingTabs({ current }: { current: View }) {
  const { account } = useWorkspace();
  return (
    <nav className="toolbox-tabs area-tabs" aria-label="Device groups">
      {TABS.filter(({ view }) => canView(account, view)).map(({ view, label, icon: Icon }) => (
        <Link key={view} to={VIEW_PATHS[view]} aria-current={current === view ? "page" : undefined}>
          <Icon size={16} aria-hidden="true" />{label}
        </Link>
      ))}
    </nav>
  );
}
