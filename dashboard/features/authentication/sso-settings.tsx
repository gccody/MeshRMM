import { CircleAlert, LoaderCircle, LogIn, Plus, RefreshCw, Trash2, X } from "lucide-react";
import { type FormEvent, useCallback, useState } from "react";
import { AuthenticationRequired, RequestError, errorText, expectJson, expectOk, jsonBody } from "../../lib/http";
import { useResource } from "../../lib/use-resource";
import { type Role, sortRoles } from "../users/model";
import { useWorkspace } from "../workspace/workspace-context";
import { CopyField } from "./copy-field";
import { DEFAULT_SCOPES, MAX_SSO_NAME_LENGTH, type SsoForm, type SsoSettings, ssoForm, ssoFormProblem, ssoUpdate } from "./model";

// Signing in through an OpenID Connect identity provider: Okta, Microsoft
// Entra ID, Google, Keycloak and the like. Administrators only.
export function SsoSettingsSection() {
  const { authorizedFetch, refreshAccount } = useWorkspace();
  const load = useCallback(async () => {
    const [settings, roles] = await Promise.all([
      authorizedFetch("/v1/settings/sso").then((response) => expectJson<SsoSettings>(response, "The single sign-on settings could not be loaded.")),
      authorizedFetch("/v1/roles").then((response) => expectJson<Role[]>(response, "Roles could not be loaded.")),
    ]);
    return { settings, roles: sortRoles(roles) };
  }, [authorizedFetch]);
  const { data, error: loadError, reload, setData } = useResource(load, "The single sign-on settings could not be loaded.");
  const saved = data?.settings ?? null;
  const [source, setSource] = useState<SsoSettings | null>(null);
  const [form, setForm] = useState<SsoForm>(() => ssoForm(null));
  const [busy, setBusy] = useState<"save" | "remove" | null>(null);
  const [actionError, setError] = useState<string | null>(null);
  const error = actionError ?? loadError;
  // The provider's discovery document didn't load, so it wasn't turned on.
  const [discoveryError, setDiscoveryError] = useState<string | null>(null);
  const [notice, setNotice] = useState<string | null>(null);

  // Loaded or saved settings replace the form's values.
  if (saved !== source) {
    setSource(saved);
    if (saved) setForm(ssoForm(saved.provider));
  }

  const edit = (change: Partial<SsoForm>) => setForm((current) => ({ ...current, ...change }));
  const editMapping = (index: number, change: Partial<SsoForm["groupRoles"][number]>) =>
    setForm((current) => ({ ...current, groupRoles: current.groupRoles.map((mapping, at) => (at === index ? { ...mapping, ...change } : mapping)) }));

  const run = async (action: "save" | "remove", work: () => Promise<string>) => {
    setBusy(action);
    setError(null);
    setDiscoveryError(null);
    setNotice(null);
    try {
      setNotice(await work());
    } catch (failure) {
      if (failure instanceof AuthenticationRequired) return;
      if (failure instanceof RequestError && failure.code === "discovery_failed") setDiscoveryError(failure.message);
      else setError(errorText(failure, "That didn't work. Try again."));
    } finally {
      setBusy(null);
    }
  };

  const problem = ssoFormProblem(form);

  const save = (event: FormEvent) => {
    event.preventDefault();
    if (problem) {
      setError(problem);
      return;
    }
    void run("save", async () => {
      const updated = await expectJson<SsoSettings>(await authorizedFetch("/v1/settings/sso", jsonBody(ssoUpdate(form), "PUT")), "The single sign-on settings could not be saved.");
      setData((current) => ({ ...current, settings: updated }));
      // The sign-in page shows the provider's button once SSO is on.
      void refreshAccount().catch(() => {});
      return updated.provider?.enabled ? "Single sign-on is on. Try it from a private window before relying on it." : "Single sign-on settings saved. It's off until you turn it on.";
    });
  };

  const remove = () => {
    if (!window.confirm("Remove single sign-on? Nobody can sign in with it any more, and roles from SSO groups end. Accounts stay, with any password or passkeys they have.")) return;
    void run("remove", async () => {
      await expectOk(await authorizedFetch("/v1/settings/sso", { method: "DELETE" }), "Single sign-on could not be removed.");
      setData((current) => ({ ...current, settings: { ...current.settings, provider: null } }));
      void refreshAccount().catch(() => {});
      return "Single sign-on was removed.";
    });
  };

  const provider = saved?.provider ?? null;
  const roles = data?.roles ?? [];

  return (
    <section className="management-panel">
      <div className="management-heading">
        <h2><LogIn size={15} aria-hidden="true" /> Single sign-on (OpenID Connect)</h2>
        <p>Sign in through Okta, Entra ID, Google or any OpenID Connect provider.</p>
      </div>
      {!saved ? (
        error
          ? <><p role="alert" className="form-error">{error}</p><button type="button" className="secondary-button" onClick={reload}><RefreshCw size={15} /> Try again</button></>
          : <p role="status" className="field-help"><LoaderCircle size={14} className="spin" /> Loading…</p>
      ) : (
        <>
          <p className={`two-factor-status ${provider?.enabled ? "on" : "off"}`}>{provider ? (provider.enabled ? `On: “Sign in with ${provider.display_name}”` : "Set up, but off.") : "Not set up."}</p>
          <div className="form-stack form-narrow">
            <CopyField label="Redirect URI" value={saved.redirect_uri} />
            <small className="field-help">Register this in your provider’s OpenID Connect app.</small>
          </div>
          <form className="form-stack form-narrow sso-form" onSubmit={save}>
            <fieldset className="form-stack" disabled={busy !== null}>
              <label className="checkbox-row">
                <input type="checkbox" checked={form.enabled} onChange={(event) => edit({ enabled: event.target.checked })} />
                <strong>Turn on single sign-on</strong>
              </label>
              <label htmlFor="sso-name">Provider name
                <input id="sso-name" required maxLength={MAX_SSO_NAME_LENGTH} placeholder="Okta" value={form.displayName} onChange={(event) => edit({ displayName: event.target.value })} />
              </label>
              <label htmlFor="sso-issuer">Issuer URL
                <input id="sso-issuer" type="url" required placeholder="https://example.okta.com" value={form.issuerUrl} onChange={(event) => edit({ issuerUrl: event.target.value })} />
              </label>
              <label htmlFor="sso-client-id">Client ID
                <input id="sso-client-id" required autoComplete="off" spellCheck={false} value={form.clientId} onChange={(event) => edit({ clientId: event.target.value })} />
              </label>
              <label htmlFor="sso-client-secret">Client secret
                <input
                  id="sso-client-secret"
                  type="password"
                  autoComplete="new-password"
                  placeholder={provider?.has_client_secret && !form.removeSecret ? "Stored — leave blank to keep" : ""}
                  value={form.clientSecret}
                  onChange={(event) => edit({ clientSecret: event.target.value })}
                  aria-describedby={provider?.has_client_secret && !form.clientSecret ? undefined : "sso-client-secret-help"}
                />
              </label>
              {provider?.has_client_secret && !form.clientSecret ? (
                <label className="checkbox-row"><input type="checkbox" checked={form.removeSecret} onChange={(event) => edit({ removeSecret: event.target.checked })} /><span>Remove the stored secret</span></label>
              ) : (
                <small id="sso-client-secret-help" className="field-help">Leave empty for a public client.</small>
              )}
              <label htmlFor="sso-scopes">Scopes
                <input id="sso-scopes" autoComplete="off" spellCheck={false} placeholder={DEFAULT_SCOPES} value={form.scopes} onChange={(event) => edit({ scopes: event.target.value })} />
              </label>
              <label className="checkbox-row">
                <input type="checkbox" checked={form.requireVerifiedEmail} onChange={(event) => edit({ requireVerifiedEmail: event.target.checked })} />
                <span><strong>Require a verified email address</strong>Turn off only if you control every address the provider issues. Entra ID needs its optional xms_edov claim.</span>
              </label>
              <SsoRoleFields form={form} roles={roles} edit={edit} editMapping={editMapping} />
            </fieldset>
            {discoveryError && (
              <div className="error-banner sso-discovery-error" role="alert">
                <CircleAlert size={17} aria-hidden="true" />
                <span><strong>MeshRMM couldn&apos;t load the provider&apos;s discovery document, so single sign-on wasn&apos;t saved.</strong> {discoveryError} Check the issuer URL, and that this server can reach it.</span>
              </div>
            )}
            {error && <p role="alert" className="form-error">{error}</p>}
            {notice && <p role="status" className="form-notice">{notice}</p>}
            <div className="form-actions">
              <button className="primary-button" disabled={busy !== null}>{busy === "save" && <LoaderCircle size={16} className="spin" />} Save single sign-on</button>
              {provider && <button type="button" className="danger-button" disabled={busy !== null} onClick={remove}><Trash2 size={15} /> Remove single sign-on</button>}
            </div>
          </form>
        </>
      )}
    </section>
  );
}

// Who gets an account on first sign-in, and the roles the provider's groups grant.
function SsoRoleFields({ form, roles, edit, editMapping }: {
  form: SsoForm;
  roles: Role[];
  edit: (change: Partial<SsoForm>) => void;
  editMapping: (index: number, change: Partial<SsoForm["groupRoles"][number]>) => void;
}) {
  return (
    <>
      <label className="checkbox-row">
        <input type="checkbox" checked={form.autoProvision} onChange={(event) => edit({ autoProvision: event.target.checked })} />
        <span><strong>Create accounts on first sign-in</strong>Otherwise only invited people can use it.</span>
      </label>
      {form.autoProvision && (
        <label htmlFor="sso-default-role">Role for new accounts
          <select id="sso-default-role" value={form.defaultRoleId} onChange={(event) => edit({ defaultRoleId: event.target.value })}>
            <option value="">No role (groups may still grant one)</option>
            {roles.map((role) => <option key={role.id} value={role.id}>{role.name}</option>)}
          </select>
        </label>
      )}
      <label htmlFor="sso-groups-claim">Groups claim (optional)
        <input id="sso-groups-claim" autoComplete="off" spellCheck={false} placeholder="groups" value={form.groupsClaim} onChange={(event) => edit({ groupsClaim: event.target.value })} aria-describedby="sso-groups-claim-help" />
      </label>
      <small id="sso-groups-claim-help" className="field-help">Such as groups or realm_access.roles. Roles from groups apply only to SSO sign-ins.</small>
      {form.groupsClaim.trim() && (
        <fieldset className="group-mappings">
          <legend>Roles from groups</legend>
          {form.groupRoles.length === 0 && <p className="field-help">No groups grant a role yet.</p>}
          {form.groupRoles.map((mapping, index) => (
            <div className="group-mapping" key={index}>
              <input aria-label={`Group ${index + 1}`} placeholder="Group name" value={mapping.group} onChange={(event) => editMapping(index, { group: event.target.value })} />
              <select aria-label={`Role for group ${index + 1}`} value={mapping.role_id} onChange={(event) => editMapping(index, { role_id: event.target.value })}>
                <option value="" disabled>Choose a role</option>
                {roles.map((role) => <option key={role.id} value={role.id}>{role.name}</option>)}
              </select>
              <button type="button" className="agent-delete-button" onClick={() => edit({ groupRoles: form.groupRoles.filter((_, at) => at !== index) })} aria-label={`Remove group ${index + 1}`} title="Remove"><X size={16} /></button>
            </div>
          ))}
          <div><button type="button" className="secondary-button" onClick={() => edit({ groupRoles: [...form.groupRoles, { group: "", role_id: "" }] })}><Plus size={15} /> Add a group</button></div>
        </fieldset>
      )}
    </>
  );
}
