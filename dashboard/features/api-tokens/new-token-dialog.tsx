import { CircleAlert, LoaderCircle, X } from "lucide-react";
import { type FormEvent, type RefObject, useId, useState } from "react";
import { AuthenticationRequired, errorText, expectJson, jsonBody } from "../../lib/http";
import { ModalDialog } from "../../lib/modal-dialog";
import { CopyField } from "../authentication/copy-field";
import type { Permission } from "../auth/types";
import { groupPermissions } from "../roles/permissions";
import type { PermissionInfo } from "../users/model";
import { useWorkspace } from "../workspace/workspace-context";
import { type ApiTokenPolicy, type CreatedApiToken, defaultLifetime, lifetimeChoices, newTokenProblem } from "./model";

// Makes a token for the signed-in user, then shows its secret, once.
export function NewTokenDialog({ policy, permissions, returnFocus, onCreated, onClose }: {
  policy: ApiTokenPolicy;
  // The user's own permissions, which a token may be limited to.
  permissions: PermissionInfo[];
  returnFocus: RefObject<HTMLElement | null>;
  onCreated: (token: CreatedApiToken) => void;
  onClose: () => void;
}) {
  const { authorizedFetch } = useWorkspace();
  const titleId = useId();
  const choices = lifetimeChoices(policy.max_lifetime_days);
  const [name, setName] = useState("");
  const [lifetime, setLifetime] = useState<string>(String(defaultLifetime(policy.max_lifetime_days)));
  const [scoped, setScoped] = useState(false);
  const [chosen, setChosen] = useState<Set<Permission>>(() => new Set());
  const [touched, setTouched] = useState(false);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [created, setCreated] = useState<CreatedApiToken | null>(null);
  const problem = newTokenProblem(name, scoped, chosen);

  const toggle = (permission: Permission, checked: boolean) => {
    setChosen((current) => {
      const next = new Set(current);
      if (checked) next.add(permission);
      else next.delete(permission);
      return next;
    });
  };

  const submit = async (event: FormEvent) => {
    event.preventDefault();
    setTouched(true);
    if (problem) return;
    setBusy(true);
    setError(null);
    try {
      const body = {
        name: name.trim(),
        expires_in_days: lifetime === "never" ? null : Number(lifetime),
        permissions: scoped ? [...chosen] : null,
      };
      const token = await expectJson<CreatedApiToken>(await authorizedFetch("/v1/api-tokens", jsonBody(body)), "The token could not be made.");
      setCreated(token);
      onCreated(token);
    } catch (failure) {
      if (!(failure instanceof AuthenticationRequired)) setError(errorText(failure, "The token could not be made."));
    } finally {
      setBusy(false);
    }
  };

  return (
    <ModalDialog className="settings-modal api-token-modal" labelledBy={titleId} onClose={onClose} returnFocus={returnFocus}>
      <button type="button" className="modal-close" onClick={onClose} aria-label="Close"><X size={19} /></button>
      <h2 id={titleId}>{created ? `“${created.name}” is ready` : "New API token"}</h2>
      {created ? (
        <div className="form-stack">
          <p className="form-notice" role="status"><CircleAlert size={15} aria-hidden="true" /> Copy the token now and keep it somewhere safe, such as your script&apos;s secret store. It won&apos;t be shown again.</p>
          <CopyField label="API token" value={created.secret} />
          <p className="field-help">Send it as <code>Authorization: Bearer &lt;token&gt;</code>. It acts as you, so treat it like your password.</p>
          <div className="connection-reason-actions"><button type="button" className="primary-button" onClick={onClose}>Done</button></div>
        </div>
      ) : (
        <form onSubmit={(event) => void submit(event)}>
          <p>Scripts and integrations use a token to call the MeshRMM API as you. It can never do more than you can.</p>
          <fieldset className="script-editor-fields" disabled={busy}>
            <label htmlFor="api-token-name">Name
              <input id="api-token-name" required maxLength={80} placeholder="PSA ticket sync" value={name} onChange={(event) => setName(event.target.value)} />
            </label>
            <label htmlFor="api-token-lifetime">Expires after
              <select id="api-token-lifetime" value={lifetime} onChange={(event) => setLifetime(event.target.value)}>
                {choices.map((choice) => <option key={choice.label} value={choice.days === null ? "never" : String(choice.days)}>{choice.label}</option>)}
              </select>
            </label>
            {policy.max_lifetime_days !== null && <small className="field-help">This server allows tokens of up to {policy.max_lifetime_days} days.</small>}
            <fieldset className="choice-list run-as-choice">
              <legend>What it may do</legend>
              <label>
                <input type="radio" name="api-token-scope" checked={!scoped} onChange={() => setScoped(false)} />
                <span><strong>Everything you can do</strong>Follows your roles as they change.</span>
              </label>
              <label>
                <input type="radio" name="api-token-scope" checked={scoped} onChange={() => setScoped(true)} />
                <span><strong>Only some of it</strong>Choose the permissions it needs; it still loses any your roles lose.</span>
              </label>
            </fieldset>
            {scoped && groupPermissions(permissions).map(({ label, items }) => (
              <fieldset key={label} className="choice-list permission-group">
                <legend>{label}</legend>
                {items.map((permission) => (
                  <label key={permission.name}>
                    <input type="checkbox" checked={chosen.has(permission.name)} onChange={(event) => toggle(permission.name, event.target.checked)} />
                    <span><strong><code>{permission.name}</code></strong>{permission.description}</span>
                  </label>
                ))}
              </fieldset>
            ))}
          </fieldset>
          {touched && problem && <p role="alert" className="installer-error">{problem}</p>}
          {error && <p role="alert" className="installer-error">{error}</p>}
          <div className="connection-reason-actions">
            <button type="button" className="secondary-button" onClick={onClose}>Cancel</button>
            <button className="primary-button" disabled={busy}>{busy && <LoaderCircle size={16} className="spin" />} Make token</button>
          </div>
        </form>
      )}
    </ModalDialog>
  );
}
