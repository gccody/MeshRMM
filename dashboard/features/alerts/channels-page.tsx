import { CircleAlert, CircleCheck, LoaderCircle, Mail, Pencil, Plus, RefreshCw, Send, Trash2, Webhook, X } from "lucide-react";
import { useCallback, useRef, useState } from "react";
import { AuthenticationRequired, errorText } from "../../lib/http";
import { formatDateTime, formatRelative } from "../../lib/format";
import { useResource } from "../../lib/use-resource";
import { HeaderActions } from "../workspace/header-actions";
import { useWorkspace } from "../workspace/workspace-context";
import { deleteChannel, fetchChannels, fetchRecipients, testChannel } from "./alerts-api";
import { AlertAreaTabs } from "./area-tabs";
import { ChannelEditor } from "./channel-editor";
import type { Channel, Recipients } from "./model";

const byName = (left: Channel, right: Channel) => left.name.localeCompare(right.name, undefined, { sensitivity: "base" });

// `/notification-channels`: where alert rules send notifications.
export function ChannelsPage() {
  const { authorizedFetch } = useWorkspace();
  const [dialog, setDialog] = useState<{ channel: Channel | null } | null>(null);
  const [busyId, setBusyId] = useState<string | null>(null);
  const [actionError, setActionError] = useState<string | null>(null);
  const [notice, setNotice] = useState<string | null>(null);
  const opener = useRef<HTMLElement | null>(null);

  const load = useCallback(async () => {
    const [list, recipients] = await Promise.all([fetchChannels(authorizedFetch), fetchRecipients(authorizedFetch)]);
    return { ...list, channels: list.channels.sort(byName), recipients };
  }, [authorizedFetch]);
  const { data, error, reload, setData } = useResource(load, "The notification channels could not be loaded.");
  const replace = (saved: Channel | null, id: string) => setData((current) => ({
    ...current,
    channels: [...current.channels.filter((channel) => channel.id !== id), ...(saved ? [saved] : [])].sort(byName),
  }));

  const open = (channel: Channel | null, button: HTMLElement) => {
    opener.current = button;
    setDialog({ channel });
  };

  const test = async (channel: Channel) => {
    setBusyId(channel.id);
    setActionError(null);
    setNotice(null);
    try {
      const result = await testChannel(authorizedFetch, channel.id);
      if (result.delivered) setNotice(`The test notification reached ${channel.name}.`);
      else setActionError(`The test notification to ${channel.name} failed: ${result.error ?? "unknown error"}.`);
      // The server recorded how the test went.
      reload();
    } catch (failure) {
      if (!(failure instanceof AuthenticationRequired)) setActionError(errorText(failure, "The test could not be sent."));
    } finally {
      setBusyId(null);
    }
  };

  const remove = async (channel: Channel) => {
    if (!window.confirm(`Delete the channel “${channel.name}”? Rules that notify it stop doing so.`)) return;
    setBusyId(channel.id);
    setActionError(null);
    setNotice(null);
    try {
      await deleteChannel(authorizedFetch, channel.id);
      replace(null, channel.id);
    } catch (failure) {
      if (!(failure instanceof AuthenticationRequired)) setActionError(errorText(failure, "The channel could not be deleted."));
    } finally {
      setBusyId(null);
    }
  };

  return (
    <>
      <HeaderActions>
        {data && <button type="button" className="primary-button" onClick={(event) => open(null, event.currentTarget)} aria-haspopup="dialog"><Plus size={16} /> New channel</button>}
      </HeaderActions>
      <AlertAreaTabs current="notification-channels" />
      {actionError && (
        <div className="error-banner" role="alert">
          <CircleAlert size={17} aria-hidden="true" /><span>{actionError}</span>
          <button onClick={() => setActionError(null)} aria-label="Dismiss"><X size={16} /></button>
        </div>
      )}
      {notice && <p className="form-notice page-notice" role="status"><CircleCheck size={16} aria-hidden="true" />{notice}</p>}
      {data && !data.email_available && (
        <p className="auth-note page-notice">Email isn&apos;t set up on this server, so email channels can&apos;t deliver. An administrator can set it up under Authentication, in Email.</p>
      )}
      <section className="agent-panel">
        {!data ? (
          error
            ? <div className="empty-state"><CircleAlert size={22} aria-hidden="true" /><strong>{error}</strong><button type="button" className="secondary-button" onClick={reload}><RefreshCw size={16} /> Try again</button></div>
            : <div className="empty-state"><LoaderCircle size={22} className="spin" aria-hidden="true" /><strong>Loading channels…</strong></div>
        ) : data.channels.length === 0 ? (
          <div className="empty-state">
            <Send size={22} aria-hidden="true" />
            <strong>No notification channels yet</strong>
            <span>Email chosen users or everyone with a role, or post to a webhook, when an alert is raised. Then choose the channel in a rule.</span>
          </div>
        ) : (
          <table className="data-table" aria-label="Notification channels">
            <thead>
              <tr>
                <th scope="col">Channel</th>
                <th scope="col">Sends to</th>
                <th scope="col">Last delivery</th>
                <th scope="col"><span className="sr-only">Actions</span></th>
              </tr>
            </thead>
            <tbody>
              {data.channels.map((channel) => (
                <tr key={channel.id} className={channel.enabled ? undefined : "row-disabled"}>
                  <td>
                    <strong className="channel-name">{channel.kind === "email" ? <Mail size={15} aria-hidden="true" /> : <Webhook size={15} aria-hidden="true" />}{channel.name}{!channel.enabled && <span className="badge badge-muted">Off</span>}</strong>
                    <span>{channel.kind === "email" ? (data.email_available ? "Email" : "Email · unavailable without SMTP") : channel.has_secret ? "Webhook · signed" : "Webhook"}</span>
                  </td>
                  <td className="channel-target">{target(channel, data.recipients)}</td>
                  <td>
                    {channel.last_delivery_at === null ? <span>Never</span> : channel.last_delivery_error === null ? (
                      <><span className="run-pill run-pill-success">Delivered</span><span title={formatDateTime(channel.last_delivery_at)}>{formatRelative(channel.last_delivery_at)}</span></>
                    ) : (
                      <><span className="run-pill run-pill-problem">Failed</span><span title={formatDateTime(channel.last_delivery_at)}>{formatRelative(channel.last_delivery_at)}: {channel.last_delivery_error}</span></>
                    )}
                  </td>
                  <td className="row-actions">
                    <button type="button" className="secondary-button" disabled={busyId === channel.id} onClick={() => void test(channel)}>{busyId === channel.id ? <LoaderCircle size={15} className="spin" /> : <Send size={15} />} Send test</button>
                    <button type="button" className="close-session-button" onClick={(event) => open(channel, event.currentTarget)} aria-haspopup="dialog" aria-label={`Edit ${channel.name}`} title="Edit channel"><Pencil size={16} /></button>
                    <button type="button" className="agent-delete-button" disabled={busyId === channel.id} onClick={() => void remove(channel)} aria-label={`Delete ${channel.name}`} title="Delete channel"><Trash2 size={16} /></button>
                  </td>
                </tr>
              ))}
            </tbody>
          </table>
        )}
      </section>
      {dialog && data && (
        <ChannelEditor
          channel={dialog.channel}
          recipients={data.recipients}
          emailAvailable={data.email_available}
          returnFocus={opener}
          onClose={() => setDialog(null)}
          onSaved={(saved) => {
            replace(saved, saved.id);
            setDialog(null);
          }}
        />
      )}
    </>
  );
}

// Who an email channel reaches, or a webhook's host.
function target(channel: Channel, recipients: Recipients) {
  if (channel.kind === "webhook") {
    try {
      return new URL(channel.webhook_url ?? "").host;
    } catch {
      return channel.webhook_url;
    }
  }
  const roles = channel.role_ids.map((id) => recipients.roles.find((role) => role.id === id)?.name).filter(Boolean).map((name) => `Everyone in ${name}`);
  const users = channel.user_ids.map((id) => recipients.users.find((user) => user.id === id)?.display_name).filter(Boolean);
  const all = [...roles, ...users];
  return all.length ? all.join(", ") : "Nobody";
}
