import { Building2, Tags } from "lucide-react";
import { Link } from "react-router";
import { VIEW_PATHS, type View } from "../workspace/views";

const TABS: { view: View; label: string; icon: typeof Building2 }[] = [
  { view: "companies", label: "Companies", icon: Building2 },
  { view: "tags", label: "Tags", icon: Tags },
];

// The two ways of grouping devices, as links above each page.
export function GroupingTabs({ current }: { current: View }) {
  return (
    <nav className="toolbox-tabs area-tabs" aria-label="Device groups">
      {TABS.map(({ view, label, icon: Icon }) => (
        <Link key={view} to={VIEW_PATHS[view]} aria-current={current === view ? "page" : undefined}>
          <Icon size={16} aria-hidden="true" />{label}
        </Link>
      ))}
    </nav>
  );
}
