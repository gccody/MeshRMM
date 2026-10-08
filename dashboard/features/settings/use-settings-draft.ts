import { useCallback, useEffect, useState } from "react";
import { isOpeningExternalLink } from "../session/viewer-launch";
import {
  type GeneralSettings,
  type SettingsDraft as Draft,
  type SettingsTab,
  draftFromSettings,
  draftMatchesSettings,
} from "./general-settings";

export type SettingsDraft = ReturnType<typeof useSettingsDraft>;

// Unsaved settings. The workspace shell owns them, so an edit survives moving
// to another page and back; only closing or reloading the tab loses it.
export function useSettingsDraft(settings: GeneralSettings | null) {
  const [settingsTab, setSettingsTab] = useState<SettingsTab>("general");
  const [draft, setDraft] = useState(() => draftFromSettings(settings));
  const [draftSource, setDraftSource] = useState(settings);

  // Loaded or saved settings replace any unsaved edits.
  if (settings !== draftSource) {
    setDraftSource(settings);
    setDraft(draftFromSettings(settings));
  }

  const updateDraft = useCallback(
    (change: Partial<Draft>) => setDraft((current) => ({ ...current, ...change })),
    [],
  );
  const isDirty = Boolean(settings) && !draftMatchesSettings(draft, settings);

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
