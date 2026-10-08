// What the toolbox lets a user do, from their permissions. Private items need
// the permission to use their kind; shared ones the permission to manage it.
// Imports carry the .ts extension so node:test can load this module directly.
import type { Permission } from "../auth/types.ts";

export type ToolboxTab = "scripts" | "files" | "runs";

export function toolboxAccess(can: (permission: Permission) => boolean) {
  const runScripts = can("scripts.run");
  const shareScripts = can("scripts.manage_shared");
  const deliverFiles = can("files.deliver");
  const shareFiles = can("files.manage_shared");
  const tabs: ToolboxTab[] = [];
  if (runScripts || shareScripts) tabs.push("scripts");
  if (deliverFiles || shareFiles) tabs.push("files");
  // Everyone's runs are in the audit trail; one's own come with scripts.run.
  if (runScripts || can("audit.view")) tabs.push("runs");
  return {
    tabs,
    runScripts,
    addScripts: runScripts || shareScripts,
    shareScripts,
    keepPrivateScripts: runScripts,
    addFiles: deliverFiles || shareFiles,
    shareFiles,
    keepPrivateFiles: deliverFiles,
  };
}

// Whether a new or edited item may, must, or can't be shared.
export type Sharing = "optional" | "required" | "unavailable";
