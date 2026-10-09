
import { LoaderCircle, RefreshCw } from "lucide-react";
import { type FormEvent, useEffect, useState } from "react";
import { AuthenticationRequired, errorText, expectJson, jsonBody } from "../../lib/http";
import { CategoryTabs } from "../workspace/category-tabs";
import { useWorkspace } from "../workspace/workspace-context";
import {
  DEFAULT_BLACKOUT_MESSAGE,
  DEFAULT_CONNECTION_APPROVAL_MESSAGE,
  DEFAULT_CONNECTION_NOTIFICATION_MESSAGE,
  IDLE_DISCONNECT_MINUTES,
  MAX_CONNECTION_APPROVAL_LOCK_IDLE_SECONDS,
  MAX_CONNECTION_APPROVAL_TIMEOUT_SECONDS,
  MAX_INSTANCE_NAME_LENGTH,
  MIN_CONNECTION_APPROVAL_TIMEOUT_SECONDS,
  SETTINGS_TABS,
  type GeneralSettings,
  type SettingsDraft,
  settingsBody,
  draftMatchesSettings,
  formatIdleDisconnect,
  isBlackoutMessageValid,
  isConnectionApprovalDraftValid,
  isConnectionApprovalLockIdleValid,
  isConnectionApprovalMessageValid,
  isConnectionApprovalTimeoutValid,
  isConnectionNotificationMessageValid,
  isInstanceNameValid,
} from "./general-settings";

// A cleared number field holds NaN in the draft, which the input shows empty.
const numberInputValue = (value: number) => (Number.isNaN(value) ? "" : value);

// The server's name and the remote session policy. The workspace shell
// keeps the loaded settings and the unsaved draft, so edits survive moving
// to another page and back.
export function SettingsPanel() {
  const { account, authorizedFetch, refreshAccount, settings, setSettings, settingsDraft } = useWorkspace();
  const displayName = account.user.display_name;
  const { draft, updateDraft, settingsTab, setSettingsTab } = settingsDraft;
  const [settingsNotice, setSettingsNotice] = useState<string | null>(null);
  const [saveError, setSaveError] = useState<string | null>(null);
  const [loadError, setLoadError] = useState<string | null>(null);
  const [loadAttempt, setLoadAttempt] = useState(0);
  const [isSaving, setIsSaving] = useState(false);

  // Loads once per visit to the workspace; the shell keeps them after that.
  useEffect(() => {
    if (settings) return;
    let cancelled = false;
    authorizedFetch("/v1/settings")
      .then((response) => expectJson<GeneralSettings>(response, "The settings could not be loaded."))
      .then(
        (loaded) => {
          if (!cancelled) setSettings(loaded);
        },
        (error: unknown) => {
          if (!cancelled && !(error instanceof AuthenticationRequired)) setLoadError(errorText(error, "The settings could not be loaded."));
        },
      );
    return () => {
      cancelled = true;
    };
  }, [authorizedFetch, loadAttempt, setSettings, settings]);

  const instanceNameValid = isInstanceNameValid(draft.instanceName);

  const blackoutMessageValid = isBlackoutMessageValid(draft.blackoutMessage);
  const notificationMessageValid = isConnectionNotificationMessageValid(draft.connectionNotificationMessage);
  const approvalValid = isConnectionApprovalDraftValid(draft);

  const saveSettings = async (event: FormEvent) => {
    event.preventDefault();
    setIsSaving(true);
    setSettingsNotice(null);
    setSaveError(null);
    try {
      const response = await authorizedFetch("/v1/settings", jsonBody(settingsBody(draft), "PATCH"));
      setSettings(await expectJson<GeneralSettings>(response, "The settings could not be saved."));
      setSettingsNotice("Settings saved.");
      // The server's name and idle timeout reach every page through these.
      void refreshAccount().catch(() => {});
    } catch (requestError) {
      if (!(requestError instanceof AuthenticationRequired)) {
        setSaveError(errorText(requestError, "The settings could not be saved."));
      }
    } finally {
      setIsSaving(false);
    }
  };

  return (
    <div className="settings-page">
      <CategoryTabs label="Settings categories" idPrefix="settings" tabs={SETTINGS_TABS} selected={settingsTab} onSelect={setSettingsTab} />
      {!settings ? (
        loadError
          ? <div className="management-panel account-status"><p role="alert">{loadError}</p><button type="button" className="secondary-button" onClick={() => { setLoadError(null); setLoadAttempt((attempt) => attempt + 1); }}><RefreshCw size={16} /> Try again</button></div>
          : <p role="status">Loading settings…</p>
      ) : <>
        <form className="company-settings-form" onSubmit={saveSettings}>
          <fieldset disabled={isSaving}>
            <section className="settings-section" id="settings-general" role="tabpanel" aria-labelledby="settings-tab-general" hidden={settingsTab !== "general"} tabIndex={0}>
              <h2>General</h2>
              <p>The name appears here, in emails and in authenticator apps.</p>
              <label htmlFor="instance-name">Server name<input id="instance-name" required maxLength={MAX_INSTANCE_NAME_LENGTH} value={draft.instanceName} onChange={(event) => updateDraft({ instanceName: event.target.value })} aria-invalid={!instanceNameValid} />
              </label>
              <label htmlFor="idle-timeout">Sign out browsers inactive for<select id="idle-timeout" value={draft.idleTimeoutMinutes} onChange={(event) => updateDraft({ idleTimeoutMinutes: Number(event.target.value) })}>
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
            <section className="settings-section" id="settings-remote-sessions" role="tabpanel" aria-labelledby="settings-tab-remote-sessions" hidden={settingsTab !== "remote-sessions"} tabIndex={0}>
              <h2>Remote sessions</h2>
              <p>Defaults for new connections.</p>
              <label>
                <input type="checkbox" checked={draft.displayBorder} onChange={(event) => updateDraft({ displayBorder: event.target.checked })} /> Highlight the viewed monitor on the device</label>
              <label>
                <input type="checkbox" checked={draft.preventIdleLock} onChange={(event) => updateDraft({ preventIdleLock: event.target.checked })} /> Keep devices from locking while idle</label>
              <label className="setting-sub">
                <input type="checkbox" checked={draft.allowIdleOverride} onChange={(event) => updateDraft({ allowIdleOverride: event.target.checked })} /> Let technicians change this per session</label>
              <label htmlFor="idle-disconnect">Disconnect idle technicians after<select id="idle-disconnect" value={draft.idleDisconnectMinutes ?? ""} onChange={(event) => updateDraft({ idleDisconnectMinutes: event.target.value === "" ? null : Number(event.target.value) })} aria-describedby="idle-disconnect-help">
                <option value="">{formatIdleDisconnect(null)}</option>
                {IDLE_DISCONNECT_MINUTES.map((minutes) => <option key={minutes} value={minutes}>{formatIdleDisconnect(minutes)}</option>)}
              </select>
              </label>
              <p id="idle-disconnect-help">No keyboard, mouse or chat input for this long ends the session.</p>
              <label className="setting-sub">
                <input type="checkbox" checked={draft.allowIdleDisconnectOverride} onChange={(event) => updateDraft({ allowIdleDisconnectOverride: event.target.checked })} aria-describedby="idle-disconnect-help" /> Let technicians change this per session</label>
              <label>
                <input type="checkbox" checked={draft.clearClipboardOnClose} onChange={(event) => updateDraft({ clearClipboardOnClose: event.target.checked })} /> Clear the device’s clipboard when a session ends</label>
              <label className="setting-sub">
                <input type="checkbox" checked={draft.allowClearClipboardOverride} onChange={(event) => updateDraft({ allowClearClipboardOverride: event.target.checked })} /> Let technicians change this per session</label>
              <label>
                <input type="checkbox" checked={draft.sessionBanner} onChange={(event) => updateDraft({ sessionBanner: event.target.checked })} aria-describedby="session-banner-help" /> Show a banner on the device while connected</label>
              <p id="session-banner-help">Technicians can’t turn it off. Chat still appears without it.</p>
            </section>
            <section className="settings-section" id="settings-blackout" role="tabpanel" aria-labelledby="settings-tab-blackout" hidden={settingsTab !== "blackout"} tabIndex={0}>
              <h2>Blackout message</h2>
              <p>Shown on the device while its screen is blacked out.</p>
              <label htmlFor="blackout-message">Message<textarea id="blackout-message" rows={4} required maxLength={2048} value={draft.blackoutMessage} onChange={(event) => updateDraft({ blackoutMessage: event.target.value })} aria-describedby="blackout-message-help" />
              </label>
              <p id="blackout-message-help">{"{user_name}"} becomes the technician’s name. Up to 2 KB.</p>
              <div className="blackout-preview" aria-label="Blackout message preview">{draft.blackoutMessage.replaceAll("{user_name}", displayName)}</div>
              <button type="button" className="secondary-button" onClick={() => updateDraft({ blackoutMessage: DEFAULT_BLACKOUT_MESSAGE })}>Restore default message</button>
            </section>
            <section className="settings-section" id="settings-connection-notification" role="tabpanel" aria-labelledby="settings-tab-connection-notification" hidden={settingsTab !== "connection-notification"} tabIndex={0}>
              <h2>Connection notification</h2>
              <p>Shown on the device’s main monitor when a technician connects.</p>
              <label>
                <input type="checkbox" checked={draft.connectionNotification} onChange={(event) => updateDraft({ connectionNotification: event.target.checked })} /> Notify the device’s user when a technician connects</label>
              <label>
                <input type="checkbox" checked={draft.backgroundConnectionNotification} onChange={(event) => updateDraft({ backgroundConnectionNotification: event.target.checked })} /> Also notify for background connections</label>
                                          <label htmlFor="connection-notification-message">Message<textarea id="connection-notification-message" rows={3} required maxLength={512} value={draft.connectionNotificationMessage} onChange={(event) => updateDraft({ connectionNotificationMessage: event.target.value })} aria-describedby="connection-notification-message-help" />
              </label>
              <p id="connection-notification-message-help">{"{user_name}"} becomes the technician’s name. Up to 512 bytes.</p>
              <div className="connection-notification-preview" aria-label="Connection notification preview">
                <strong>Remote session started</strong>
                <span>{draft.connectionNotificationMessage.replaceAll("{user_name}", displayName)}</span>
              </div>
              <button type="button" className="secondary-button" onClick={() => updateDraft({ connectionNotificationMessage: DEFAULT_CONNECTION_NOTIFICATION_MESSAGE })}>Restore default message</button>
            </section>
            <ConnectionApprovalSection draft={draft} updateDraft={updateDraft} displayName={displayName} hidden={settingsTab !== "connection-approval"} />
          </fieldset>
          {settingsTab !== "general" && !instanceNameValid && <p role="alert">Check the server name before saving: it needs 1 to {MAX_INSTANCE_NAME_LENGTH} characters.</p>}
          {settingsTab !== "blackout" && !blackoutMessageValid && <p role="alert">Check the blackout message before saving: it must contain text and be no larger than 2 KB.</p>}
          {settingsTab !== "connection-notification" && !notificationMessageValid && <p role="alert">Check the connection notification message before saving: it must contain text and be no larger than 512 bytes.</p>}
          {settingsTab !== "connection-approval" && !approvalValid && <p role="alert">Check the connection approval settings before saving.</p>}
          <div className="settings-save">
            {saveError && <p className="settings-save-error" role="alert">{saveError}</p>}
            <button className="primary-button" disabled={isSaving || !instanceNameValid || !blackoutMessageValid || !notificationMessageValid || !approvalValid || draftMatchesSettings(draft, settings)}>{isSaving && <LoaderCircle size={16} className="spin" />} Save settings</button>
          </div>
          {settingsNotice && <p role="status" className="session-notice">{settingsNotice}</p>}
        </form>
      </>}
    </div>
  );
}

function ConnectionApprovalSection({ draft, updateDraft, displayName, hidden }: {
  draft: SettingsDraft;
  updateDraft: (change: Partial<SettingsDraft>) => void;
  displayName: string;
  hidden: boolean;
}) {
  return (
    <section className="settings-section" id="settings-connection-approval" role="tabpanel" aria-labelledby="settings-tab-connection-approval" hidden={hidden} tabIndex={0}>
      <h2>Connection approval</h2>
      <p>The device’s user accepts or denies each connection first.</p>
      <label>
        <input type="checkbox" checked={draft.connectionApproval} onChange={(event) => updateDraft({ connectionApproval: event.target.checked })} aria-describedby="connection-approval-help" /> Ask the device’s user to approve each connection</label>
      <p id="connection-approval-help">Includes background connections. Reconnecting to the same session doesn’t ask again.</p>
      <label htmlFor="connection-approval-message">Message<textarea id="connection-approval-message" rows={3} required maxLength={512} value={draft.connectionApprovalMessage} onChange={(event) => updateDraft({ connectionApprovalMessage: event.target.value })} aria-describedby="connection-approval-message-help" />
      </label>
      <p id="connection-approval-message-help">{"{user_name}"} becomes the technician’s name; their reason appears below. Up to 512 bytes.</p>
      {!isConnectionApprovalMessageValid(draft.connectionApprovalMessage) && <p role="alert">The prompt message must contain text and be no larger than 512 bytes.</p>}
      <div className="connection-approval-preview" aria-label="Connection approval preview">
        <strong>Remote connection request</strong>
        <span>{draft.connectionApprovalMessage.replaceAll("{user_name}", displayName)}</span>
        <span className="connection-approval-reason">Reason: Checking the printer queue</span>
        <small>Accepts automatically in {numberInputValue(draft.connectionApprovalTimeoutSeconds)} seconds.</small>
        <span className="connection-approval-buttons" aria-hidden="true"><span>Deny</span><span>Accept</span></span>
      </div>
      <button type="button" className="secondary-button" onClick={() => updateDraft({ connectionApprovalMessage: DEFAULT_CONNECTION_APPROVAL_MESSAGE })}>Restore default message</button>
      <label htmlFor="connection-approval-timeout">Accept automatically after (seconds)<input id="connection-approval-timeout" type="number" inputMode="numeric" required min={MIN_CONNECTION_APPROVAL_TIMEOUT_SECONDS} max={MAX_CONNECTION_APPROVAL_TIMEOUT_SECONDS} step={1} value={numberInputValue(draft.connectionApprovalTimeoutSeconds)} onChange={(event) => updateDraft({ connectionApprovalTimeoutSeconds: event.target.valueAsNumber })} aria-describedby="connection-approval-timeout-help" aria-invalid={!isConnectionApprovalTimeoutValid(draft.connectionApprovalTimeoutSeconds)} />
      </label>
      <p id="connection-approval-timeout-help">{MIN_CONNECTION_APPROVAL_TIMEOUT_SECONDS} to {MAX_CONNECTION_APPROVAL_TIMEOUT_SECONDS} seconds without an answer.</p>
      <label htmlFor="connection-approval-lock-idle">Accept at once when locked and idle for (seconds)<input id="connection-approval-lock-idle" type="number" inputMode="numeric" required min={0} max={MAX_CONNECTION_APPROVAL_LOCK_IDLE_SECONDS} step={1} value={numberInputValue(draft.connectionApprovalLockIdleSeconds)} onChange={(event) => updateDraft({ connectionApprovalLockIdleSeconds: event.target.valueAsNumber })} aria-describedby="connection-approval-lock-idle-help" aria-invalid={!isConnectionApprovalLockIdleValid(draft.connectionApprovalLockIdleSeconds)} />
      </label>
      <p id="connection-approval-lock-idle-help">0 to {MAX_CONNECTION_APPROVAL_LOCK_IDLE_SECONDS} seconds. 0 accepts whenever the device is locked or nobody is signed in.</p>
    </section>
  );
}
