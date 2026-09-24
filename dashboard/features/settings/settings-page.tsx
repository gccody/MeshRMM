"use client";

import { Clock3, LoaderCircle } from "lucide-react";
import { type FormEvent, useState } from "react";
import { AuthenticationRequired, type AuthorizedFetch, errorMessage } from "../../lib/http";
import type { Account, Company } from "../workspace/types";
import {
  type CompanySettingsDraft,
  DEFAULT_BLACKOUT_MESSAGE,
  SETTINGS_TABS,
  type SettingsTab,
  companySettingsBody,
  draftFromCompany,
  draftMatchesCompany,
  isBlackoutMessageValid,
  settingsTabForKey,
} from "./company-settings";

type Props = {
  company: Company | null | undefined;
  isAdmin: boolean;
  displayName: string;
  authorizedFetch: AuthorizedFetch;
  onSaved: (account: Account) => void;
  reportError: (message: string | null) => void;
};

export function SettingsPage({ company, isAdmin, displayName, authorizedFetch, onSaved, reportError }: Props) {
  const [settingsTab, setSettingsTab] = useState<SettingsTab>("dashboard-security");
  const [draft, setDraft] = useState(() => draftFromCompany(company));
  const [draftSource, setDraftSource] = useState(company);
  const [settingsNotice, setSettingsNotice] = useState<string | null>(null);
  const [isSaving, setIsSaving] = useState(false);

  // A loaded or saved account replaces any unsaved edits.
  if (company !== draftSource) {
    setDraftSource(company);
    setDraft(draftFromCompany(company));
  }

  const updateDraft = (change: Partial<CompanySettingsDraft>) => setDraft((current) => ({ ...current, ...change }));
  const blackoutMessageValid = isBlackoutMessageValid(draft.blackoutMessage);

  const saveSettings = async (event: FormEvent) => {
    event.preventDefault();
    setIsSaving(true);
    setSettingsNotice(null);
    reportError(null);
    try {
      const response = await authorizedFetch("/v1/company/settings", {
        method: "PUT",
        headers: { "Content-Type": "application/json" },
        body: JSON.stringify(companySettingsBody(draft)),
      });
      if (!response.ok) {
        throw new Error(await errorMessage(response, "The session policy could not be saved."));
      }
      onSaved((await response.json()) as Account);
      setSettingsNotice("Company settings saved. Remote defaults apply to new sessions.");
    } catch (requestError) {
      if (!(requestError instanceof AuthenticationRequired)) {
        reportError(requestError instanceof Error ? requestError.message : "The session policy could not be saved.");
      }
    } finally {
      setIsSaving(false);
    }
  };

  return (
    <div className="settings-page">
      <div className="settings-categories" role="tablist" aria-label="Settings categories">
        {SETTINGS_TABS.map((tab, index) => (
          <button
            key={tab.id}
            type="button"
            role="tab"
            id={`settings-tab-${tab.id}`}
            aria-controls={tab.id}
            aria-selected={settingsTab === tab.id}
            tabIndex={settingsTab === tab.id ? 0 : -1}
            onClick={() => setSettingsTab(tab.id)}
            onKeyDown={(event) => {
              const next = settingsTabForKey(index, event.key);
              if (next === null) return;
              event.preventDefault();
              setSettingsTab(SETTINGS_TABS[next].id);
              document.getElementById(`settings-tab-${SETTINGS_TABS[next].id}`)?.focus();
            }}
          >{tab.label}</button>
        ))}
      </div>
      {!company ? <p role="status">Loading company settings…</p> : <>
        {!isAdmin && <p className="session-notice">Company settings are managed by your administrator.</p>}
        <form className="company-settings-form" onSubmit={saveSettings}>
          <fieldset disabled={!isAdmin || isSaving}>
            <section className="settings-section" id="dashboard-security" role="tabpanel" aria-labelledby="settings-tab-dashboard-security" hidden={settingsTab !== "dashboard-security"} tabIndex={0}>
              <h2>Dashboard security</h2>
              <p>Choose when an inactive dashboard session is paused.</p>
              <label htmlFor="idle-timeout">Sign out inactive dashboards after<select id="idle-timeout" value={draft.idleTimeoutMinutes} onChange={(event) => updateDraft({ idleTimeoutMinutes: Number(event.target.value) })}>
                <option value={5}>5 minutes</option>
                <option value={15}>15 minutes</option>
                <option value={30}>30 minutes</option>
                <option value={60}>1 hour</option>
                <option value={120}>2 hours</option>
                <option value={240}>4 hours</option>
                <option value={480}>8 hours</option>
                <option value={720}>12 hours</option>
                <option value={1440}>24 hours</option>
              </select>
              </label>
            </section>
            <section className="settings-section" id="remote-sessions" role="tabpanel" aria-labelledby="settings-tab-remote-sessions" hidden={settingsTab !== "remote-sessions"} tabIndex={0}>
              <h2>Remote sessions</h2>
              <p>Defaults for new connections. Monitor highlighting can be changed in the viewer.</p>
              <label>
                <input type="checkbox" checked={draft.displayBorder} onChange={(event) => updateDraft({ displayBorder: event.target.checked })} /> Highlight the viewed monitor on the agent’s physical display by default</label>
              <label>
                <input type="checkbox" checked={draft.preventIdleLock} onChange={(event) => updateDraft({ preventIdleLock: event.target.checked })} /> Prevent remote devices from locking while idle by default</label>
              <label>
                <input type="checkbox" checked={draft.allowIdleOverride} onChange={(event) => updateDraft({ allowIdleOverride: event.target.checked })} /> Allow users to change idle-lock prevention per session</label>
              <p>The per-session permission above applies only to idle-lock prevention.</p>
            </section>
            <section className="settings-section" id="blackout" role="tabpanel" aria-labelledby="settings-tab-blackout" hidden={settingsTab !== "blackout"} tabIndex={0}>
              <h2>Blackout message</h2>
              <p>Shown on the remote device when a technician enables screen blackout.</p>
              <label htmlFor="blackout-message">Agent blackout message<textarea id="blackout-message" rows={4} required maxLength={2048} value={draft.blackoutMessage} onChange={(event) => updateDraft({ blackoutMessage: event.target.value })} aria-describedby="blackout-message-help" />
              </label>
              <p id="blackout-message-help">Use {"{user_name}"} for the technician’s banner name. Applies to new remote sessions. Keep the message short (up to 2 KB).</p>
              <div className="blackout-preview" aria-label="Blackout message preview">{draft.blackoutMessage.replaceAll("{user_name}", displayName)}</div>
              <button type="button" className="secondary-button" onClick={() => updateDraft({ blackoutMessage: DEFAULT_BLACKOUT_MESSAGE })}>Restore default message</button>
            </section>
          </fieldset>
          {settingsTab !== "blackout" && !blackoutMessageValid && <p role="alert">Check the blackout message before saving: it must contain text and be no larger than 2 KB.</p>}
          {isAdmin && <div className="settings-save">
            <button className="primary-button" disabled={isSaving || !blackoutMessageValid || draftMatchesCompany(draft, company)}>{isSaving ? <LoaderCircle size={16} className="spin" /> : <Clock3 size={16} />} Save company settings</button>
          </div>}
          {settingsNotice && <p role="status" className="session-notice">{settingsNotice}</p>}
        </form>
      </>}
    </div>
  );
}
