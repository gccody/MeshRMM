import { LoaderCircle, Mail, RefreshCw, Send, ShieldCheck, Trash2 } from "lucide-react";
import { type FormEvent, useCallback, useState } from "react";
import { AuthenticationRequired, errorText, expectJson, expectOk, jsonBody } from "../../lib/http";
import { useResource } from "../../lib/use-resource";
import { useWorkspace } from "../workspace/workspace-context";
import { ScimSettingsSection } from "./scim-settings";
import { SsoSettingsSection } from "./sso-settings";

// GET /v1/settings/authentication
type AuthenticationSettings = {
  require_two_factor: boolean;
  password_min_length: number;
  session_lifetime_hours: number;
};

// GET /v1/settings/smtp
type SmtpSettings = {
  configured: boolean;
  host: string | null;
  port: number | null;
  security: SmtpSecurity;
  username: string | null;
  // The password itself is never sent back.
  has_password: boolean;
  from: string | null;
};

type SmtpSecurity = "starttls" | "tls" | "none";

const MIN_PASSWORD_LENGTH = 8;
const MAX_PASSWORD_LENGTH = 128;
const MAX_SESSION_LIFETIME_HOURS = 8760;

const SECURITY_LABELS: Record<SmtpSecurity, string> = {
  starttls: "STARTTLS (usually port 587)",
  tls: "TLS (usually port 465)",
  none: "None: unencrypted, for a relay on a trusted network",
};

// The sign-in policy, and for administrators, single sign-on, directory
// sync and how the server sends email.
export function AuthenticationPanel() {
  const { account, can } = useWorkspace();
  return (
    <div className="management-stack">
      {can("authentication.manage") && <SignInPolicy />}
      {account.is_administrator && <SsoSettingsSection />}
      {account.is_administrator && <ScimSettingsSection />}
      {account.is_administrator && <EmailSettings />}
    </div>
  );
}

function SignInPolicy() {
  const { authorizedFetch, refreshAccount } = useWorkspace();
  const load = useCallback(
    async () => expectJson<AuthenticationSettings>(await authorizedFetch("/v1/settings/authentication"), "The sign-in policy could not be loaded."),
    [authorizedFetch],
  );
  const { data: saved, error: loadError, reload, setData } = useResource(load, "The sign-in policy could not be loaded.");
  const [source, setSource] = useState<AuthenticationSettings | null>(null);
  const [requireTwoFactor, setRequireTwoFactor] = useState(false);
  const [minLength, setMinLength] = useState("");
  const [lifetime, setLifetime] = useState("");
  const [busy, setBusy] = useState(false);
  const [actionError, setError] = useState<string | null>(null);
  const error = actionError ?? loadError;
  const [notice, setNotice] = useState<string | null>(null);

  // Loaded or saved settings replace the form's values.
  if (saved !== source) {
    setSource(saved);
    if (saved) {
      setRequireTwoFactor(saved.require_two_factor);
      setMinLength(String(saved.password_min_length));
      setLifetime(String(saved.session_lifetime_hours));
    }
  }

  const minLengthValue = Number(minLength);
  const lifetimeValue = Number(lifetime);
  const minLengthValid = Number.isInteger(minLengthValue) && minLengthValue >= MIN_PASSWORD_LENGTH && minLengthValue <= MAX_PASSWORD_LENGTH;
  const lifetimeValid = Number.isInteger(lifetimeValue) && lifetimeValue >= 1 && lifetimeValue <= MAX_SESSION_LIFETIME_HOURS;
  const changed = saved !== null && (
    requireTwoFactor !== saved.require_two_factor ||
    minLengthValue !== saved.password_min_length ||
    lifetimeValue !== saved.session_lifetime_hours
  );

  const submit = async (event: FormEvent) => {
    event.preventDefault();
    if (!saved || !minLengthValid || !lifetimeValid) return;
    if (requireTwoFactor && !saved.require_two_factor && !window.confirm("Require two-factor authentication? Everyone who signs in with a password and hasn't set it up must do so before they can use MeshRMM.")) return;
    setBusy(true);
    setError(null);
    setNotice(null);
    try {
      const body = {
        require_two_factor: requireTwoFactor,
        password_min_length: minLengthValue,
        session_lifetime_hours: lifetimeValue,
      };
      const updated = await expectJson<AuthenticationSettings>(await authorizedFetch("/v1/settings/authentication", jsonBody(body, "PATCH")), "The sign-in policy could not be saved.");
      setData(() => updated);
      setNotice("Sign-in policy saved.");
      // Requiring two-factor may apply to the signed-in user too.
      await refreshAccount();
    } catch (failure) {
      if (!(failure instanceof AuthenticationRequired)) setError(errorText(failure, "The sign-in policy could not be saved."));
    } finally {
      setBusy(false);
    }
  };

  return (
    <section className="management-panel">
      <div className="management-heading"><h2><ShieldCheck size={15} aria-hidden="true" /> Sign-in policy</h2><p>Applies to everyone who signs in with a password.</p></div>
      {!saved ? (
        error
          ? <><p role="alert" className="form-error">{error}</p><button type="button" className="secondary-button" onClick={reload}><RefreshCw size={15} /> Try again</button></>
          : <p role="status" className="field-help"><LoaderCircle size={14} className="spin" /> Loading…</p>
      ) : (
        <form className="form-stack form-narrow" onSubmit={(event) => void submit(event)}>
          <label className="checkbox-row">
            <input type="checkbox" checked={requireTwoFactor} onChange={(event) => setRequireTwoFactor(event.target.checked)} />
            <span><strong>Require two-factor authentication</strong>Users without an authenticator app or passkey must add one the next time they sign in, before anything else. Single sign-on leaves two-factor to the identity provider.</span>
          </label>
          <label htmlFor="password-min-length">Minimum password length
            <input id="password-min-length" type="number" inputMode="numeric" min={MIN_PASSWORD_LENGTH} max={MAX_PASSWORD_LENGTH} step={1} required value={minLength} onChange={(event) => setMinLength(event.target.value)} aria-invalid={!minLengthValid} aria-describedby="password-min-length-help" />
          </label>
          <small id="password-min-length-help" className="field-help">Between {MIN_PASSWORD_LENGTH} and {MAX_PASSWORD_LENGTH} characters. Applies to new passwords; existing ones keep working.</small>
          <label htmlFor="session-lifetime">Sign everyone in again after (hours)
            <input id="session-lifetime" type="number" inputMode="numeric" min={1} max={MAX_SESSION_LIFETIME_HOURS} step={1} required value={lifetime} onChange={(event) => setLifetime(event.target.value)} aria-invalid={!lifetimeValid} aria-describedby="session-lifetime-help" />
          </label>
          <small id="session-lifetime-help" className="field-help">How long a sign-in lasts, however active the user is: 1 to {MAX_SESSION_LIFETIME_HOURS} hours. Applies to sign-ins from now on. Inactive browsers sign out sooner, as set in Settings.</small>
          {error && <p role="alert" className="form-error">{error}</p>}
          {notice && <p role="status" className="form-notice">{notice}</p>}
          <div><button className="primary-button" disabled={busy || !changed || !minLengthValid || !lifetimeValid}>{busy && <LoaderCircle size={16} className="spin" />} Save sign-in policy</button></div>
        </form>
      )}
    </section>
  );
}

function EmailSettings() {
  const { account, authorizedFetch, refreshAccount } = useWorkspace();
  const load = useCallback(
    async () => expectJson<SmtpSettings>(await authorizedFetch("/v1/settings/smtp"), "The email settings could not be loaded."),
    [authorizedFetch],
  );
  const { data: saved, error: loadError, reload, setData } = useResource(load, "The email settings could not be loaded.");
  const [source, setSource] = useState<SmtpSettings | null>(null);
  const [host, setHost] = useState("");
  const [port, setPort] = useState("");
  const [security, setSecurity] = useState<SmtpSecurity>("starttls");
  const [username, setUsername] = useState("");
  const [password, setPassword] = useState("");
  const [clearPassword, setClearPassword] = useState(false);
  const [from, setFrom] = useState("");
  const [testTo, setTestTo] = useState(account.user.email);
  const [busy, setBusy] = useState<"save" | "remove" | "test" | null>(null);
  const [actionError, setError] = useState<string | null>(null);
  const error = actionError ?? loadError;
  const [notice, setNotice] = useState<string | null>(null);

  // Loaded or saved settings replace the form's values.
  if (saved !== source) {
    setSource(saved);
    if (saved) {
      setHost(saved.host ?? "");
      setPort(saved.port === null ? "" : String(saved.port));
      setSecurity(saved.security);
      setUsername(saved.username ?? "");
      setPassword("");
      setClearPassword(false);
      setFrom(saved.from ?? "");
    }
  }

  const run = async (action: "save" | "remove" | "test", work: () => Promise<string>) => {
    setBusy(action);
    setError(null);
    setNotice(null);
    try {
      setNotice(await work());
    } catch (failure) {
      if (!(failure instanceof AuthenticationRequired)) setError(errorText(failure, "That didn't work. Try again."));
    } finally {
      setBusy(null);
    }
  };

  const save = (event: FormEvent) => {
    event.preventDefault();
    void run("save", async () => {
      const body: Record<string, unknown> = {
        host: host.trim(),
        security,
        username: username.trim() || null,
        from: from.trim(),
      };
      if (port.trim()) body.port = Number(port);
      // A missing password keeps the stored one; null removes it.
      if (password) body.password = password;
      else if (clearPassword || !username.trim()) body.password = null;
      const updated = await expectJson<SmtpSettings>(await authorizedFetch("/v1/settings/smtp", jsonBody(body, "PUT")), "The email settings could not be saved.");
      setData(() => updated);
      // The sign-in page offers emailed reset links once email works.
      void refreshAccount().catch(() => {});
      return "Email settings saved. Send a test email to check them.";
    });
  };

  const remove = () => {
    if (!window.confirm("Turn off email? Invitations and password reset links will be shown to administrators to pass on instead.")) return;
    void run("remove", async () => {
      await expectOk(await authorizedFetch("/v1/settings/smtp", { method: "DELETE" }), "Email could not be turned off.");
      void refreshAccount().catch(() => {});
      setData(() => ({ configured: false, host: null, port: null, security: "starttls", username: null, has_password: false, from: null }));
      return "Email is off.";
    });
  };

  const test = () => void run("test", async () => {
    await expectOk(await authorizedFetch("/v1/settings/smtp/test", jsonBody({ to: testTo.trim() || undefined })), "The test email failed.");
    return `Sent a test email to ${testTo.trim() || account.user.email}.`;
  });

  return (
    <section className="management-panel">
      <div className="management-heading">
        <h2><Mail size={15} aria-hidden="true" /> Email</h2>
        <p>MeshRMM emails invitations and password reset links through your SMTP server. Without it, administrators copy the links and pass them on.</p>
      </div>
      {!saved ? (
        error
          ? <><p role="alert" className="form-error">{error}</p><button type="button" className="secondary-button" onClick={reload}><RefreshCw size={15} /> Try again</button></>
          : <p role="status" className="field-help"><LoaderCircle size={14} className="spin" /> Loading…</p>
      ) : (
        <>
          <p className={`two-factor-status ${saved.configured ? "on" : "off"}`}>{saved.configured ? "Email is set up." : "Email is off."}</p>
          <form className="form-stack form-narrow" onSubmit={save}>
            <fieldset className="form-stack" disabled={busy !== null}>
              <label htmlFor="smtp-host">SMTP server<input id="smtp-host" required placeholder="smtp.example.com" value={host} onChange={(event) => setHost(event.target.value)} /></label>
              <label htmlFor="smtp-security">Encryption
                <select id="smtp-security" value={security} onChange={(event) => setSecurity(event.target.value as SmtpSecurity)}>
                  {(Object.keys(SECURITY_LABELS) as SmtpSecurity[]).map((value) => <option key={value} value={value}>{SECURITY_LABELS[value]}</option>)}
                </select>
              </label>
              <label htmlFor="smtp-port">Port (optional)<input id="smtp-port" type="number" inputMode="numeric" min={1} max={65535} placeholder={security === "tls" ? "465" : security === "starttls" ? "587" : "25"} value={port} onChange={(event) => setPort(event.target.value)} /></label>
              <label htmlFor="smtp-username">Username (optional)<input id="smtp-username" autoComplete="off" value={username} onChange={(event) => setUsername(event.target.value)} /></label>
              <label htmlFor="smtp-password">Password
                <input id="smtp-password" type="password" autoComplete="new-password" placeholder={saved.has_password && !clearPassword ? "Saved; leave empty to keep it" : ""} value={password} onChange={(event) => setPassword(event.target.value)} disabled={!username.trim()} />
              </label>
              {saved.has_password && username.trim() && !password && (
                <label className="checkbox-row"><input type="checkbox" checked={clearPassword} onChange={(event) => setClearPassword(event.target.checked)} /><span>Remove the saved password</span></label>
              )}
              <label htmlFor="smtp-from">Send as<input id="smtp-from" required placeholder="MeshRMM <rmm@example.com>" value={from} onChange={(event) => setFrom(event.target.value)} /></label>
            </fieldset>
            {error && <p role="alert" className="form-error">{error}</p>}
            {notice && <p role="status" className="form-notice">{notice}</p>}
            <div className="form-actions">
              <button className="primary-button" disabled={busy !== null}>{busy === "save" && <LoaderCircle size={16} className="spin" />} Save email settings</button>
              {saved.configured && <button type="button" className="danger-button" disabled={busy !== null} onClick={remove}><Trash2 size={15} /> Turn off email</button>}
            </div>
          </form>
          {saved.configured && (
            <div className="form-stack form-narrow smtp-test">
              <label htmlFor="smtp-test-to">Send a test email to<input id="smtp-test-to" type="email" value={testTo} onChange={(event) => setTestTo(event.target.value)} /></label>
              <div><button type="button" className="secondary-button" disabled={busy !== null} onClick={test}>{busy === "test" ? <LoaderCircle size={15} className="spin" /> : <Send size={15} />} Send test email</button></div>
            </div>
          )}
        </>
      )}
    </section>
  );
}
