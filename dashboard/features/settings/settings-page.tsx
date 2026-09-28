"use client";

import { Clock3, LoaderCircle } from "lucide-react";
import { type FormEvent, useState } from "react";
import { AuthenticationRequired, type AuthorizedFetch, errorMessage } from "../../lib/http";
import type { Account, Company } from "../workspace/types";
import {
  DEFAULT_BLACKOUT_MESSAGE,
  DEFAULT_CONNECTION_NOTIFICATION_MESSAGE,
  SETTINGS_TABS,
  companySettingsBody,
  draftMatchesCompany,
  isBlackoutMessageValid,
  isConnectionNotificationMessageValid,
  settingsTabForKey,
} from "./company-settings";
import type { SettingsDraft } from "./use-settings-draft";

type Props = {
  company: Company | null | undefined;
  isAdmin: boolean;
  displayName: string;
  authorizedFetch: AuthorizedFetch;
  onSaved: (account: Account) => void;
  // Owned by the workspace shell so unsaved edits survive navigation.
  settingsDraft: SettingsDraft;
};

export function SettingsPage({ company, isAdmin, displayName, authorizedFetch, onSaved, settingsDraft }: Props) {
  const { draft, updateDraft, settingsTab, setSettingsTab } = settingsDraft;
  const [settingsNotice, setSettingsNotice] = useState<string | null>(null);
  const [saveError, setSaveError] = useState<string | null>(null);
  const [isSaving, setIsSaving] = useState(false);

  const blackoutMessageValid = isBlackoutMessageValid(draft.blackoutMessage);
  const notificationMessageValid = isConnectionNotificationMessageValid(draft.connectionNotificationMessage);

  const saveSettings = async (event: FormEvent) => {
    event.preventDefault();
    setIsSaving(true);
    setSettingsNotice(null);
    setSaveError(null);
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
        setSaveError(requestError instanceof Error ? requestError.message : "The session policy could not be saved.");
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
              <label>
                <input type="checkbox" checked={draft.sessionBanner} onChange={(event) => updateDraft({ sessionBanner: event.target.checked })} aria-describedby="session-banner-help" /> Show a banner on the agent’s screen while a technician is connected</label>
              <p id="session-banner-help">Applies to every new session and cannot be changed by users. Chat messages still appear on the agent when the banner is hidden.</p>
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
            <section className="settings-section" id="connection-notification" role="tabpanel" aria-labelledby="settings-tab-connection-notification" hidden={settingsTab !== "connection-notification"} tabIndex={0}>
              <h2>Connection notification</h2>
              <p>Shown in the corner of the remote device’s main monitor when a technician connects.</p>
              <label>
                <input type="checkbox" checked={draft.connectionNotification} onChange={(event) => updateDraft({ connectionNotification: event.target.checked })} /> Notify the agent’s user when a technician connects to their session</label>
              <label>
                <input type="checkbox" checked={draft.backgroundConnectionNotification} onChange={(event) => updateDraft({ backgroundConnectionNotification: event.target.checked })} aria-describedby="background-notification-help" /> Also notify the user when a technician connects in background mode</label>
              <p id="background-notification-help">Background mode works on a separate desktop the user cannot see. If a technician switches from it to the user’s session, the first option applies.</p>
              <p>Both apply to every new session and cannot be changed by users. The notification closes when clicked or after 15 seconds.</p>
              <label htmlFor="connection-notification-message">Notification message<textarea id="connection-notification-message" rows={3} required maxLength={512} value={draft.connectionNotificationMessage} onChange={(event) => updateDraft({ connectionNotificationMessage: event.target.value })} aria-describedby="connection-notification-message-help" />
              </label>
              <p id="connection-notification-message-help">Use {"{user_name}"} for the technician’s banner name. Applies to new remote sessions. Keep the message short (up to 512 bytes).</p>
              <div className="connection-notification-preview" aria-label="Connection notification preview">
                <strong>Remote session started</strong>
                <span>{draft.connectionNotificationMessage.replaceAll("{user_name}", displayName)}</span>
              </div>
              <button type="button" className="secondary-button" onClick={() => updateDraft({ connectionNotificationMessage: DEFAULT_CONNECTION_NOTIFICATION_MESSAGE })}>Restore default message</button>
            </section>
          </fieldset>
          {settingsTab !== "blackout" && !blackoutMessageValid && <p role="alert">Check the blackout message before saving: it must contain text and be no larger than 2 KB.</p>}
          {settingsTab !== "connection-notification" && !notificationMessageValid && <p role="alert">Check the connection notification message before saving: it must contain text and be no larger than 512 bytes.</p>}
          {isAdmin && <div className="settings-save">
            {saveError && <p className="settings-save-error" role="alert">{saveError}</p>}
            <button className="primary-button" disabled={isSaving || !blackoutMessageValid || !notificationMessageValid || draftMatchesCompany(draft, company)}>{isSaving ? <LoaderCircle size={16} className="spin" /> : <Clock3 size={16} />} Save company settings</button>
          </div>}
          {settingsNotice && <p role="status" className="session-notice">{settingsNotice}</p>}
        </form>
      </>}
    </div>
  );
}
