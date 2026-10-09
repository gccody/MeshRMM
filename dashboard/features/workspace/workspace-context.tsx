import { createContext, useContext } from "react";
import type { AuthorizedFetch } from "../../lib/http";
import type { Agent } from "../agents/types";
import type { useAgentInventory } from "../agents/use-agent-inventory";
import type { Account, Permission } from "../auth/types";
import type { useRemoteHandoff } from "../session/use-remote-handoff";
import type { GeneralSettings } from "../settings/general-settings";
import type { SettingsDraft } from "../settings/use-settings-draft";
import type { ActionErrorSource, ActionErrors } from "./action-errors";

// What the persistent workspace shell shares with the page beneath it. The
// shell owns everything that must outlive a page change: the live inventory,
// the remote handoff, and the unsaved settings draft.
export type Workspace = {
  account: Account;
  instanceName: string;
  can: (permission: Permission) => boolean;
  // Reads the account again after a change to the user's own access.
  refreshAccount: () => Promise<void>;
  authorizedFetch: AuthorizedFetch;
  inventory: ReturnType<typeof useAgentInventory>;
  remote: ReturnType<typeof useRemoteHandoff>;
  deleteAgent: (agent: Agent) => Promise<void>;
  deletingId: string | null;
  // Errors from device actions, per source; a null message clears one source.
  actionErrors: ActionErrors;
  reportActionError: (source: ActionErrorSource, message: string | null) => void;
  // The Devices filters as a "?…" search string, kept for the Devices link.
  devicesSearch: string;
  setDevicesSearch: (search: string) => void;
  // The saved settings, once the Settings page loads them, and the edits.
  settings: GeneralSettings | null;
  setSettings: (settings: GeneralSettings) => void;
  settingsDraft: SettingsDraft;
  // Where HeaderActions puts a page's controls, beside its title.
  headerSlot: HTMLElement | null;
};

export const WorkspaceContext = createContext<Workspace | null>(null);

export function useWorkspace() {
  const workspace = useContext(WorkspaceContext);
  if (!workspace) throw new Error("The workspace is unavailable.");
  return workspace;
}
