"use client";

import { createContext, useContext } from "react";
import type { AuthorizedFetch } from "../../lib/http";
import type { Agent } from "../agents/types";
import type { useAgentInventory } from "../agents/use-agent-inventory";
import type { useRemoteHandoff } from "../session/use-remote-handoff";
import type { SettingsDraft } from "../settings/use-settings-draft";
import type { ActionErrorSource, ActionErrors } from "./action-errors";
import type { Account, Company } from "./types";

// What the persistent workspace shell shares with the page beneath it. The
// shell owns everything that must outlive a page change: the account, the live
// inventory, the remote handoff, and the unsaved settings draft.
export type Workspace = {
  account: Account | null;
  company: Company | null | undefined;
  setAccount: (account: Account) => void;
  isAdmin: boolean;
  displayName: string;
  authorizedFetch: AuthorizedFetch;
  getAccessToken: () => Promise<string>;
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
  settingsDraft: SettingsDraft;
};

export const WorkspaceContext = createContext<Workspace | null>(null);

export function useWorkspace() {
  const workspace = useContext(WorkspaceContext);
  if (!workspace) throw new Error("The company workspace is unavailable.");
  return workspace;
}
