import { LoaderCircle, X } from "lucide-react";
import { type FormEvent, type RefObject, useId, useState } from "react";
import { AuthenticationRequired, errorText } from "../../lib/http";
import { ModalDialog } from "../../lib/modal-dialog";
import { useWorkspace } from "../workspace/workspace-context";
import { saveChannel } from "./alerts-api";
import { type Channel, type ChannelForm, type Recipients, channelForm, channelFormProblem, channelInput } from "./model";

// Creates a channel, or edits one. A channel's kind can't change once saved.
export function ChannelEditor({ channel, recipients, emailAvailable, returnFocus, onClose, onSaved }: {
  channel: Channel | null;
  recipients: Recipients;
  emailAvailable: boolean;
  returnFocus: RefObject<HTMLElement | null>;
  onClose: () => void;
  onSaved: (channel: Channel) => void;
}) {
  const { authorizedFetch } = useWorkspace();
  const titleId = useId();
  const [form, setForm] = useState<ChannelForm>(() => channelForm(channel, emailAvailable));
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const problem = channelFormProblem(form);
  const update = (change: Partial<ChannelForm>) => setForm((current) => ({ ...current, ...change }));
  const toggle = (list: "userIds" | "roleIds", id: string, checked: boolean) => {
    setForm((current) => ({
      ...current,
      [list]: checked ? [...current[list], id] : current[list].filter((candidate) => candidate !== id),
    }));
  };

  const submit = async (event: FormEvent) => {
    event.preventDefault();
    if (problem) return;
    setBusy(true);
    setError(null);
    try {
      onSaved(await saveChannel(authorizedFetch, channel?.id ?? null, channelInput(form)));
    } catch (failure) {
      if (!(failure instanceof AuthenticationRequired)) setError(errorText(failure, "The channel could not be saved."));
      setBusy(false);
    }
  };

  return (
    <ModalDialog className="settings-modal role-modal" labelledBy={titleId} onClose={onClose} returnFocus={returnFocus}>
      <button type="button" className="modal-close" onClick={onClose} aria-label="Close"><X size={19} /></button>
      <h2 id={titleId}>{channel ? `Edit ${channel.name}` : "New notification channel"}</h2>
      <form onSubmit={(event) => void submit(event)}>
        <fieldset className="script-editor-fields" disabled={busy}>
          <label htmlFor="channel-name">Name<input id="channel-name" required maxLength={80} value={form.name} onChange={(event) => update({ name: event.target.value })} placeholder="On-call team" /></label>
          {!channel && (
            <fieldset className="run-as-choice">
              <legend>Kind</legend>
              <label>
                <input type="radio" name="channel-kind" checked={form.kind === "email"} disabled={!emailAvailable} onChange={() => update({ kind: "email" })} />
                <span><strong>Email</strong>{emailAvailable ? "To chosen users, or everyone with a role." : "Unavailable: email isn't set up on this server."}</span>
              </label>
              <label>
                <input type="radio" name="channel-kind" checked={form.kind === "webhook"} onChange={() => update({ kind: "webhook" })} />
                <span><strong>Webhook</strong>A JSON POST to a URL, such as a chat service or ticketing system.</span>
              </label>
            </fieldset>
          )}

          {form.kind === "email" ? (
            <>
              {channel && !emailAvailable && <p className="field-help field-problem">Email isn&apos;t set up on this server, so this channel can&apos;t deliver.</p>}
              <fieldset className="choice-list">
                <legend>Everyone with these roles</legend>
                {recipients.roles.map((role) => (
                  <label key={role.id}>
                    <input type="checkbox" checked={form.roleIds.includes(role.id)} onChange={(event) => toggle("roleIds", role.id, event.target.checked)} />
                    <strong>{role.name}</strong>
                  </label>
                ))}
              </fieldset>
              <fieldset className="choice-list">
                <legend>These users</legend>
                {recipients.users.map((user) => (
                  <label key={user.id}>
                    <input type="checkbox" checked={form.userIds.includes(user.id)} onChange={(event) => toggle("userIds", user.id, event.target.checked)} />
                    <span><strong>{user.display_name}</strong>{user.email}</span>
                  </label>
                ))}
              </fieldset>
              <p className="field-help">Each recipient gets their own email. Disabled accounts get nothing.</p>
            </>
          ) : (
            <>
              <label htmlFor="channel-url">URL<input id="channel-url" type="url" required maxLength={2048} value={form.url} onChange={(event) => update({ url: event.target.value })} placeholder="https://hooks.example.com/meshrmm" /></label>
              {channel?.has_secret ? (
                <fieldset className="choice-list">
                  <legend>Signing secret</legend>
                  <label><input type="radio" name="secret-mode" checked={form.secretMode === "keep"} onChange={() => update({ secretMode: "keep" })} /><strong>Keep the saved secret</strong></label>
                  <label><input type="radio" name="secret-mode" checked={form.secretMode === "replace"} onChange={() => update({ secretMode: "replace" })} /><strong>Replace it</strong></label>
                  <label><input type="radio" name="secret-mode" checked={form.secretMode === "remove"} onChange={() => update({ secretMode: "remove" })} /><span><strong>Remove it</strong>Requests are no longer signed.</span></label>
                </fieldset>
              ) : null}
              {form.secretMode === "replace" && (
                <label htmlFor="channel-secret">{channel?.has_secret ? "New signing secret" : "Signing secret (optional)"}
                  <input id="channel-secret" type="password" autoComplete="new-password" maxLength={256} value={form.secret} onChange={(event) => update({ secret: event.target.value })} />
                </label>
              )}
              <p className="field-help">
                With a secret, each request carries <code>X-MeshRMM-Signature: sha256=…</code>, the HMAC-SHA256 of the <code>X-MeshRMM-Timestamp</code> header, a dot and the body. Failed deliveries are tried again three times over about six minutes.
              </p>
            </>
          )}
          <label className="checkbox-row">
            <input type="checkbox" checked={form.enabled} onChange={(event) => update({ enabled: event.target.checked })} />
            <span><strong>Turned on</strong>A channel that&apos;s off receives nothing but tests.</span>
          </label>
        </fieldset>
        {(error || (problem && form.name.trim())) && <p role="alert" className="installer-error">{error ?? problem}</p>}
        <div className="connection-reason-actions">
          <button type="button" className="secondary-button" onClick={onClose}>Cancel</button>
          <button className="primary-button" disabled={busy || problem !== null}>{busy && <LoaderCircle size={16} className="spin" />} {channel ? "Save channel" : "Create channel"}</button>
        </div>
      </form>
    </ModalDialog>
  );
}
