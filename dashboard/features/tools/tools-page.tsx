import { ArrowLeft, CircleAlert, Cog, FolderTree, LoaderCircle, MonitorOff, Workflow } from "lucide-react";
import { Suspense, lazy, useEffect } from "react";
import { Link, useSearchParams } from "react-router";
import type { Agent } from "../agents/types";
import { useDevice } from "../agents/use-devices";
import { useWorkspace } from "../workspace/workspace-context";
import { TOOLS, TOOL_LABELS, type Tool, canManageTool, canViewTool, parseTool, toolsPagePath, unavailableReason } from "./model";

// The panels load when the page opens; they are a chunk of their own.
const ToolPanel = lazy(() => import("./tools-panels"));

// `/tools?id=…&tool=…`: a device's processes, services and registry, without
// a remote session.
export function ToolsPage() {
  const { inventory, can, setPageTitle } = useWorkspace();
  const [params] = useSearchParams();
  const deviceId = params.get("id") ?? "";
  const agent = useDevice(inventory.devices, deviceId);
  const visible = TOOLS.filter((tool) => canViewTool(can, tool));
  const tool = parseTool(params.get("tool")) ?? visible[0] ?? "processes";

  useEffect(() => {
    setPageTitle(agent ? `${TOOL_LABELS[tool]} on ${agent.name}` : null);
    return () => setPageTitle(null);
  }, [agent, tool, setPageTitle]);

  if (inventory.hasData && !agent) {
    return (
      <div className="empty-state">
        <MonitorOff size={22} aria-hidden="true" />
        <strong>Device not found</strong>
        <span>It may have been deleted.</span>
        <Link className="secondary-button" to="/">Back to devices</Link>
      </div>
    );
  }
  if (!agent) {
    return <div className="empty-state compact"><LoaderCircle size={22} className="spin" aria-hidden="true" /><strong>Loading the device…</strong></div>;
  }
  const unavailable = unavailableReason(tool, agent.platform);
  return (
    <div className="tools-page">
      <div className="device-status">
        <Link to={`/device?id=${encodeURIComponent(agent.id)}`} className="back-link"><ArrowLeft size={15} aria-hidden="true" />{agent.name}</Link>
      </div>
      <ToolTabs agent={agent} current={tool} visible={visible} />
      {!canViewTool(can, tool) ? (
        <div className="empty-state compact"><CircleAlert size={22} aria-hidden="true" /><strong>You can’t use this tool</strong><span>Your role doesn’t include seeing {TOOL_LABELS[tool].toLowerCase()}.</span></div>
      ) : unavailable ? (
        <div className="empty-state compact"><CircleAlert size={22} aria-hidden="true" /><strong>{tool === "registry" ? "There’s no registry here" : `${TOOL_LABELS[tool]} aren’t available here`}</strong><span>{unavailable}</span></div>
      ) : !agent.connected ? (
        <div className="empty-state compact"><MonitorOff size={22} aria-hidden="true" /><strong>The device is offline</strong><span>The tools work again when it reconnects.</span></div>
      ) : (
        <Suspense fallback={<div className="empty-state compact"><LoaderCircle size={22} className="spin" aria-hidden="true" /><strong>Loading…</strong></div>}>
          <ToolPanel key={`${agent.id}-${tool}`} agent={agent} tool={tool} canManage={canManageTool(can, tool)} />
        </Suspense>
      )}
    </div>
  );
}

const TAB_ICONS: Record<Tool, typeof Cog> = { processes: Workflow, services: Cog, registry: FolderTree };

// The tools, as tabs. One the device's platform lacks shows, disabled, with
// why.
export function ToolTabs({ agent, current, visible }: { agent: Agent; current: Tool; visible: readonly Tool[] }) {
  return (
    <nav className="toolbox-tabs area-tabs" aria-label="Remote tools">
      {visible.map((tool) => {
        const reason = unavailableReason(tool, agent.platform);
        const Icon = TAB_ICONS[tool];
        return reason ? (
          <span key={tool} className="tool-tab-unavailable" aria-disabled="true" title={reason}>
            <Icon size={16} aria-hidden="true" />{TOOL_LABELS[tool]}<em>{reason}</em>
          </span>
        ) : (
          <Link key={tool} to={toolsPagePath(agent.id, tool)} replace aria-current={tool === current ? "page" : undefined}>
            <Icon size={16} aria-hidden="true" />{TOOL_LABELS[tool]}
          </Link>
        );
      })}
    </nav>
  );
}

