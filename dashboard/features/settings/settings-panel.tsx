"use client";

import { useWorkspace } from "../workspace/workspace-context";
import { SettingsPage } from "./settings-page";

export function SettingsPanel() {
  const { company, isAdmin, displayName, authorizedFetch, setAccount, reportError, settingsDraft } = useWorkspace();
  return (
    <SettingsPage
      company={company}
      isAdmin={isAdmin}
      displayName={displayName}
      authorizedFetch={authorizedFetch}
      onSaved={setAccount}
      reportError={reportError}
      settingsDraft={settingsDraft}
    />
  );
}
