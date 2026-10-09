import { KeyRound, LoaderCircle, Plus, RefreshCw, Trash2, Users } from "lucide-react";
import { type FormEvent, useCallback, useState } from "react";
import { AuthenticationRequired, errorText, expectJson, expectOk, jsonBody } from "../../lib/http";
import { formatDateTime, formatRelative } from "../../lib/format";
import { useResource } from "../../lib/use-resource";
import { type Role, sortRoles } from "../users/model";
import { useWorkspace } from "../workspace/workspace-context";
import { CopyField } from "./copy-field";
import type { CreatedScimToken, ScimGroup, ScimSettings } from "./model";

// Directory sync: the identity provider creates, changes and deactivates
// accounts, and pushes groups that grant roles. Administrators only.
export function ScimSettingsSection() {
  const { authorizedFetch, refreshAccount } = useWorkspace();
  const load = useCallback(async () => {
    const [settings, roles] = await Promise.all([
      authorizedFetch("/v1/settings/scim").then((response) => expectJson<ScimSettings>(response, "The directory sync settings could not be loaded.")),
      authorizedFetch("/v1/roles").then((response) => expectJson<Role[]>(response, "Roles could not be loaded.")),
    ]);
    return { settings, roles: sortRoles(roles) };
  }, [authorizedFetch]);
  const { data, error: loadError, reload, setData } = useResource(load, "The directory sync settings could not be loaded.");
  const [name, setName] = useState("");
  // A token just made; its secret is never shown again.
  const [created, setCreated] = useState<CreatedScimToken | null>(null);
  const [busy, setBusy] = useState<string | null>(null);
  const [actionError, setError] = useState<string | null>(null);
  const error = actionError ?? loadError;

  const run = async (action: string, work: () => Promise<void>) => {
    setBusy(action);
    setError(null);
    try {
      await work();
    } catch (failure) {
      if (!(failure instanceof AuthenticationRequired)) setError(errorText(failure, "That didn't work. Try again."));
    } finally {
      setBusy(null);
    }
  };

  const createToken = (event: FormEvent) => {
    event.preventDefault();
    void run("create", async () => {
      const token = await expectJson<CreatedScimToken>(await authorizedFetch("/v1/settings/scim/tokens", jsonBody({ name: name.trim() })), "The token could not be made.");
      const listed = { id: token.id, name: token.name, created_at: token.created_at, last_used_at: token.last_used_at };
      setData((current) => ({ ...current, settings: { ...current.settings, tokens: [...current.settings.tokens, listed] } }));
      setCreated(token);
      setName("");
    });
  };

  const revoke = (id: string, tokenName: string) => {
    if (!window.confirm(`Revoke the token “${tokenName}”? An identity provider using it can't sync any more.`)) return;
    void run(id, async () => {
      await expectOk(await authorizedFetch(`/v1/settings/scim/tokens/${encodeURIComponent(id)}`, { method: "DELETE" }), "The token could not be revoked.");
      setData((current) => ({ ...current, settings: { ...current.settings, tokens: current.settings.tokens.filter((token) => token.id !== id) } }));
      if (created?.id === id) setCreated(null);
    });
  };

  const setGroupRole = (group: ScimGroup, roleId: string) => void run(group.id, async () => {
    const updated = await expectJson<ScimGroup>(
      await authorizedFetch(`/v1/settings/scim/groups/${encodeURIComponent(group.id)}`, jsonBody({ role_id: roleId || null }, "PATCH")),
      "The group's role could not be saved.",
    );
    setData((current) => ({ ...current, settings: { ...current.settings, groups: current.settings.groups.map((candidate) => (candidate.id === group.id ? updated : candidate)) } }));
    // The administrator may be in the group.
    await refreshAccount();
  });

  const settings = data?.settings ?? null;
  const roles = data?.roles ?? [];

  return (
    <section className="management-panel">
      <div className="management-heading">
        <h2><Users size={15} aria-hidden="true" /> Directory sync (SCIM)</h2>
        <p>Let your identity provider create, update and disable accounts.</p>
      </div>
      {!settings ? (
        error
          ? <><p role="alert" className="form-error">{error}</p><button type="button" className="secondary-button" onClick={reload}><RefreshCw size={15} /> Try again</button></>
          : <p role="status" className="field-help"><LoaderCircle size={14} className="spin" /> Loading…</p>
      ) : (
        <div className="form-stack scim-settings">
          <div className="form-stack form-narrow">
            <CopyField label="SCIM base URL" value={settings.base_url} />
            <ul className="field-help scim-notes">
              <li>Enter this URL and a token in your provider&apos;s SCIM app.</li>
              <li>userName must be the user&apos;s email address.</li>
            </ul>
          </div>

          <div className="scim-block">
            <h3><KeyRound size={15} aria-hidden="true" /> Tokens</h3>
            {settings.tokens.length > 0 ? (
              <ul className="session-list">
                {settings.tokens.map((token) => (
                  <li key={token.id}>
                    <div>
                      <strong>{token.name}</strong>
                      <span>Made {formatDateTime(token.created_at)} · {token.last_used_at === null ? "never used" : `last used ${formatRelative(token.last_used_at)}`}</span>
                    </div>
                    <button type="button" className="danger-button" disabled={busy !== null} onClick={() => revoke(token.id, token.name)} aria-label={`Revoke ${token.name}`}>{busy === token.id ? <LoaderCircle size={15} className="spin" /> : <Trash2 size={15} />} Revoke</button>
                  </li>
                ))}
              </ul>
            ) : <p className="field-help">No tokens yet.</p>}
            {created && (
              <div className="one-time-link">
                <p className="form-notice" role="status">Copy the token “{created.name}” into your identity provider now. It won&apos;t be shown again.</p>
                <CopyField label="SCIM token" value={created.secret} />
                <div><button type="button" className="secondary-button" onClick={() => setCreated(null)}>I&apos;ve copied it</button></div>
              </div>
            )}
            <form className="form-stack form-narrow" onSubmit={createToken}>
              <label htmlFor="scim-token-name">New token&apos;s name
                <input id="scim-token-name" required maxLength={120} placeholder="Okta" value={name} onChange={(event) => setName(event.target.value)} />
              </label>
              <div><button className="secondary-button" disabled={busy !== null || !name.trim()}>{busy === "create" ? <LoaderCircle size={15} className="spin" /> : <Plus size={15} />} Make a token</button></div>
            </form>
          </div>

          <div className="scim-block">
            <h3><Users size={15} aria-hidden="true" /> Groups</h3>
            {settings.groups.length > 0 ? (
              <table className="data-table scim-groups" aria-label="SCIM groups">
                <thead><tr><th scope="col">Group</th><th scope="col">Members</th><th scope="col">Grants</th></tr></thead>
                <tbody>
                  {settings.groups.map((group) => (
                    <tr key={group.id}>
                      <td><strong>{group.display_name}</strong>{group.external_id && <span>{group.external_id}</span>}</td>
                      <td>{group.member_count}</td>
                      <td>
                        <select className="scim-group-role" aria-label={`Role ${group.display_name} grants`} value={group.role_id ?? ""} disabled={busy !== null} onChange={(event) => setGroupRole(group, event.target.value)}>
                          <option value="">No role</option>
                          {roles.map((role) => <option key={role.id} value={role.id}>{role.name}</option>)}
                        </select>
                      </td>
                    </tr>
                  ))}
                </tbody>
              </table>
            ) : <p className="field-help">Groups appear here when your provider pushes them.</p>}
          </div>
          {error && <p role="alert" className="form-error">{error}</p>}
        </div>
      )}
    </section>
  );
}
