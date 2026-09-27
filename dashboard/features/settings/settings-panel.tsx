"use client";

import { useWorkspace } from "../workspace/workspace-context";
import { SettingsPage } from "./settings-page";

export function SettingsPanel() {
  const { company, isAdmin, displayName, authorizedFetch, setAccount, settingsDraft } = useWorkspace();
  return (
    <SettingsPage
      company={company}
      isAdmin={isAdmin}
      displayName={displayName}
      authorizedFetch={authorizedFetch}
      onSaved={setAccount}
      settingsDraft={settingsDraft}
    />
  );
}
