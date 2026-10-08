import { Fingerprint, LoaderCircle, Pencil, Plus, RefreshCw, Trash2, X } from "lucide-react";
import { type FormEvent, useCallback, useId, useState } from "react";
import { AuthenticationRequired, RequestError, errorText, expectJson, expectOk, jsonBody } from "../../lib/http";
import { formatDateTime, formatRelative } from "../../lib/format";
import { ModalDialog } from "../../lib/modal-dialog";
import { usePasskeySupport } from "../../lib/use-passkey-support";
import { useResource } from "../../lib/use-resource";
import { type CreationOptionsJSON, type PasskeyPrompt, PasskeyPromptError, createPasskey } from "../../lib/webauthn";
import { useSession } from "../auth/session";
import { useWorkspace } from "../workspace/workspace-context";
import { type AddedPasskey, MAX_PASSKEY_NAME_LENGTH, type PasskeyView, defaultPasskeyName } from "./model";
import { PasswordPrompt } from "./security-dialogs";

type Dialog = { kind: "add" } | { kind: "rename"; passkey: PasskeyView } | { kind: "remove"; passkey: PasskeyView };

// The user's passkeys, as a second factor and for signing in without a
// password. `onRecoveryCodes` gets the codes a first second factor brings.
export function PasskeysSection({ onRecoveryCodes, onNotice }: {
  onRecoveryCodes: (codes: string[]) => void;
  onNotice: (message: string) => void;
}) {
  const { account, authorizedFetch, refreshAccount } = useWorkspace();
  const { instance } = useSession();
  const supported = usePasskeySupport();
  const load = useCallback(
    async () => expectJson<PasskeyView[]>(await authorizedFetch("/v1/account/passkeys"), "Your passkeys could not be loaded."),
    [authorizedFetch],
  );
  const { data: passkeys, error, reload, setData } = useResource(load, "Your passkeys could not be loaded.");
  const [dialog, setDialog] = useState<Dialog | null>(null);
  const { two_factor: twoFactor } = account;
  // Removing the last second factor isn't allowed while the server requires one.
  const lastRequired = twoFactor.required && !twoFactor.totp && twoFactor.passkeys <= 1;

  const unavailable = !instance?.sign_in.passkey
    ? "This server's address can't be used with passkeys. An administrator can set its public URL to an https:// address with a domain name."
    : !supported
      ? "This browser can't create passkeys."
      : !account.user.has_password
        ? "Passkeys need a password on your account first. Ask an administrator for a password reset link to set one."
        : null;

  const added = async ({ passkey, recovery_codes }: AddedPasskey) => {
    setDialog(null);
    setData((current) => [...current, passkey]);
    if (recovery_codes) onRecoveryCodes(recovery_codes);
    else onNotice(`Added the passkey “${passkey.name}”.`);
    await refreshAccount().catch(() => {});
  };

  const renamed = (passkey: PasskeyView, name: string) => {
    setDialog(null);
    setData((current) => current.map((candidate) => (candidate.id === passkey.id ? { ...candidate, name } : candidate)));
  };

  const remove = async (passkey: PasskeyView, password: string) => {
    await expectOk(
      await authorizedFetch(`/v1/account/passkeys/${encodeURIComponent(passkey.id)}/remove`, jsonBody({ password })),
      "The passkey could not be removed.",
    );
    setDialog(null);
    setData((current) => current.filter((candidate) => candidate.id !== passkey.id));
    onNotice(`Removed the passkey “${passkey.name}”.`);
    await refreshAccount();
  };

  return (
    <div className="two-factor-method">
      <h3><Fingerprint size={15} aria-hidden="true" /> Passkeys</h3>
      <p className="field-help">Sign in with your fingerprint, face or screen lock, or a security key. A passkey also signs you in without your email and password.</p>
      {!passkeys ? (
        error
          ? <><p role="alert" className="form-error">{error}</p><div><button type="button" className="secondary-button" onClick={reload}><RefreshCw size={15} /> Try again</button></div></>
          : <p role="status" className="field-help"><LoaderCircle size={14} className="spin" /> Loading…</p>
      ) : (
        passkeys.length > 0 && (
          <ul className="session-list passkey-list">
            {passkeys.map((passkey) => (
              <li key={passkey.id}>
                <div>
                  <strong>{passkey.name}</strong>
                  <span>Added {formatDateTime(passkey.created_at)} · {passkey.last_used_at === null ? "never used" : `last used ${formatRelative(passkey.last_used_at)}`}</span>
                </div>
                <div className="form-actions">
                  <button type="button" className="secondary-button" onClick={() => setDialog({ kind: "rename", passkey })} aria-label={`Rename ${passkey.name}`}><Pencil size={15} /> Rename</button>
                  {!lastRequired && <button type="button" className="danger-button" onClick={() => setDialog({ kind: "remove", passkey })} aria-label={`Remove ${passkey.name}`}><Trash2 size={15} /> Remove</button>}
                </div>
              </li>
            ))}
          </ul>
        )
      )}
      {lastRequired && passkeys && passkeys.length > 0 && <p className="field-help">This server requires two-factor authentication. Set up an authenticator app or add another passkey before removing this one.</p>}
      <div><button type="button" className={twoFactor.enabled ? "secondary-button" : "primary-button"} onClick={() => setDialog({ kind: "add" })} disabled={unavailable !== null}><Plus size={15} /> Add a passkey</button></div>
      {unavailable && <p className="field-help">{unavailable}</p>}
      {dialog?.kind === "add" && <AddPasskeyDialog onAdded={added} onClose={() => setDialog(null)} />}
      {dialog?.kind === "rename" && <RenamePasskeyDialog passkey={dialog.passkey} onRenamed={renamed} onClose={() => setDialog(null)} />}
      {dialog?.kind === "remove" && (
        <PasswordPrompt
          title="Remove this passkey"
          description={`“${dialog.passkey.name}” stops working here. Also delete it from your device or password manager.${twoFactor.passkeys <= 1 && !twoFactor.totp ? " Signing in will take only your password, and your recovery codes stop working." : ""}`}
          action="Remove passkey"
          danger
          onConfirm={(password) => remove(dialog.passkey, password)}
          onClose={() => setDialog(null)}
        />
      )}
    </div>
  );
}

// A name and the user's password, then the browser's passkey prompt.
function AddPasskeyDialog({ onAdded, onClose }: { onAdded: (added: AddedPasskey) => Promise<void>; onClose: () => void }) {
  const { account, authorizedFetch } = useWorkspace();
  const id = useId();
  const [name, setName] = useState(() => defaultPasskeyName(navigator.userAgent));
  const [password, setPassword] = useState("");
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);

  const submit = async (event: FormEvent) => {
    event.preventDefault();
    setBusy(true);
    setError(null);
    try {
      const prompt = await expectJson<PasskeyPrompt<CreationOptionsJSON>>(
        await authorizedFetch("/v1/account/passkeys/options", jsonBody({ password })),
        "Adding a passkey couldn't start. Try again.",
      );
      const credential = await createPasskey(prompt.options);
      const response = await authorizedFetch("/v1/account/passkeys", jsonBody({ ceremony: prompt.ceremony, name: name.trim(), credential }));
      await onAdded(await expectJson<AddedPasskey>(response, "The passkey could not be added."));
    } catch (failure) {
      if (failure instanceof AuthenticationRequired) return;
      // Closing the browser's prompt leaves this dialog as it was.
      if (!(failure instanceof PasskeyPromptError && failure.cancelled)) {
        setError(failure instanceof RequestError && failure.code === "no_password"
          ? "Passkeys need a password on your account first. Ask an administrator for a password reset link to set one."
          : errorText(failure, "The passkey could not be added."));
      }
      setBusy(false);
    }
  };

  return (
    <ModalDialog className="settings-modal" labelledBy={`${id}-title`} onClose={busy ? () => {} : onClose}>
      <button type="button" className="modal-close" onClick={onClose} aria-label="Close" disabled={busy}><X size={19} /></button>
      <div className="modal-icon"><Fingerprint size={22} /></div>
      <h2 id={`${id}-title`}>Add a passkey</h2>
      <p>Name it after where it lives, then follow your browser&apos;s prompt. You can save it on this device, your phone or a security key.</p>
      <form onSubmit={(event) => void submit(event)}>
        <input type="email" autoComplete="username" value={account.user.email} readOnly hidden />
        <label htmlFor={`${id}-name`}>Passkey name
          <input id={`${id}-name`} required maxLength={MAX_PASSKEY_NAME_LENGTH} value={name} onChange={(event) => setName(event.target.value)} />
        </label>
        <label htmlFor={`${id}-password`}>Your password
          <input id={`${id}-password`} type="password" autoComplete="current-password" required value={password} onChange={(event) => setPassword(event.target.value)} />
        </label>
        {error && <p role="alert" className="installer-error">{error}</p>}
        <div className="connection-reason-actions">
          <button type="button" className="secondary-button" onClick={onClose} disabled={busy}>Cancel</button>
          <button className="primary-button" disabled={busy || !name.trim()}>{busy && <LoaderCircle size={16} className="spin" />} Continue</button>
        </div>
      </form>
    </ModalDialog>
  );
}

function RenamePasskeyDialog({ passkey, onRenamed, onClose }: {
  passkey: PasskeyView;
  onRenamed: (passkey: PasskeyView, name: string) => void;
  onClose: () => void;
}) {
  const { authorizedFetch } = useWorkspace();
  const id = useId();
  const [name, setName] = useState(passkey.name);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);

  const submit = async (event: FormEvent) => {
    event.preventDefault();
    setBusy(true);
    setError(null);
    try {
      await expectOk(
        await authorizedFetch(`/v1/account/passkeys/${encodeURIComponent(passkey.id)}`, jsonBody({ name: name.trim() }, "PATCH")),
        "The passkey could not be renamed.",
      );
      onRenamed(passkey, name.trim());
    } catch (failure) {
      if (failure instanceof AuthenticationRequired) return;
      setError(errorText(failure, "The passkey could not be renamed."));
      setBusy(false);
    }
  };

  return (
    <ModalDialog className="settings-modal" labelledBy={`${id}-title`} onClose={busy ? () => {} : onClose}>
      <button type="button" className="modal-close" onClick={onClose} aria-label="Close" disabled={busy}><X size={19} /></button>
      <div className="modal-icon"><Pencil size={22} /></div>
      <h2 id={`${id}-title`}>Rename passkey</h2>
      <p>The name helps you tell your passkeys apart. Only you see it.</p>
      <form onSubmit={(event) => void submit(event)}>
        <label htmlFor={`${id}-name`}>Passkey name
          <input id={`${id}-name`} required maxLength={MAX_PASSKEY_NAME_LENGTH} value={name} onChange={(event) => setName(event.target.value)} />
        </label>
        {error && <p role="alert" className="installer-error">{error}</p>}
        <div className="connection-reason-actions">
          <button type="button" className="secondary-button" onClick={onClose} disabled={busy}>Cancel</button>
          <button className="primary-button" disabled={busy || !name.trim() || name.trim() === passkey.name}>{busy && <LoaderCircle size={16} className="spin" />} Rename</button>
        </div>
      </form>
    </ModalDialog>
  );
}
