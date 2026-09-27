"use client";

import { useCallback, useEffect, useState } from "react";
import { isOpeningExternalLink } from "../session/viewer-launch";
import type { Company } from "../workspace/types";
import {
  type CompanySettingsDraft,
  type SettingsTab,
  draftFromCompany,
  draftMatchesCompany,
} from "./company-settings";

export type SettingsDraft = ReturnType<typeof useSettingsDraft>;

// Unsaved company settings. The workspace shell owns them, so an edit survives
// moving to another page and back; only closing or reloading the tab loses it.
export function useSettingsDraft(company: Company | null | undefined) {
  const [settingsTab, setSettingsTab] = useState<SettingsTab>("dashboard-security");
  const [draft, setDraft] = useState(() => draftFromCompany(company));
  const [draftSource, setDraftSource] = useState(company);

  // A loaded or saved account replaces any unsaved edits, and signing out
  // (no company) discards them.
  if (company !== draftSource) {
    setDraftSource(company);
    setDraft(draftFromCompany(company));
  }

  const updateDraft = useCallback(
    (change: Partial<CompanySettingsDraft>) => setDraft((current) => ({ ...current, ...change })),
    [],
  );
  const isDirty = Boolean(company) && !draftMatchesCompany(draft, company);

  useEffect(() => {
    if (!isDirty) return;
    const warnBeforeUnload = (event: BeforeUnloadEvent) => {
      // Opening the remote viewer's link fires beforeunload but keeps the page.
      if (!isOpeningExternalLink()) event.preventDefault();
    };
    window.addEventListener("beforeunload", warnBeforeUnload);
    return () => window.removeEventListener("beforeunload", warnBeforeUnload);
  }, [isDirty]);

  return { draft, updateDraft, isDirty, settingsTab, setSettingsTab };
}
