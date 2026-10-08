import { CircleAlert, CircleCheck, KeyRound, LoaderCircle, LogOut, MonitorSmartphone, RefreshCw, ShieldCheck, ShieldOff, Smartphone, UserRound } from "lucide-react";
import { type FormEvent, useCallback, useState } from "react";
import { AuthenticationRequired, errorText, expectJson, expectOk, jsonBody } from "../../lib/http";
import { describeUserAgent, formatDateTime, formatRelative } from "../../lib/format";
import { QrCode } from "../../lib/qr-code";
import { useResource } from "../../lib/use-resource";
import { NewPasswordFields, newPasswordProblem } from "../auth/password-fields";
import { useSession } from "../auth/session";
import { signInMethodLabel } from "../auth/sign-in";
import { useWorkspace } from "../workspace/workspace-context";
import { twoFactorSummary } from "./model";
import { PasskeysSection } from "./passkeys-section";
import { PasswordPrompt, RecoveryCodes } from "./security-dialogs";

// The signed-in user's own account. While the server requires two-factor
// authentication of a user who hasn't set it up, this is the only page open
// to them.
export function AccountPanel() {
  const { account } = useWorkspace();
  const enrolling = account.two_factor.enrollment_required;
  return (
    <div className="management-stack">
      {enrolling && (
        <div className="error-banner enrollment-banner" role="alert">
          <ShieldCheck size={17} aria-hidden="true" />
          <span>This server requires two-factor authentication. Add a passkey or set up an authenticator app below to continue.</span>
        </div>
      )}
      <TwoFactorSection />
      {!enrolling && <ProfileSection />}
      <PasswordSection />
      <SessionsSection />
    </div>
  );
}

function ProfileSection() {
  const { account, authorizedFetch, refreshAccount } = useWorkspace();
  const [name, setName] = useState(account.user.display_name);
  const [busy, setBusy] = useState(false);
  const [notice, setNotice] = useState<string | null>(null);
  const [error, setError] = useState<string | null>(null);

  const submit = async (event: FormEvent) => {
    event.preventDefault();
    setBusy(true);
    setError(null);
    setNotice(null);
    try {
      await expectOk(await authorizedFetch("/v1/account", jsonBody({ display_name: name.trim() }, "PATCH")), "Your name could not be saved.");
      await refreshAccount();
      setNotice("Your name was saved.");
    } catch (failure) {
      if (!(failure instanceof AuthenticationRequired)) setError(errorText(failure, "Your name could not be saved."));
    } finally {
      setBusy(false);
    }
  };

  return (
    <section className="management-panel">
      <div className="management-heading"><h2><UserRound size={15} aria-hidden="true" /> Profile</h2><p>Your name appears to device users in session banners and prompts.</p></div>
      <form className="form-stack form-narrow" onSubmit={(event) => void submit(event)}>
        <label htmlFor="profile-name">Name
          <input id="profile-name" autoComplete="name" required value={name} onChange={(event) => setName(event.target.value)} />
        </label>
        <label htmlFor="profile-email">Email
          <input id="profile-email" value={account.user.email} readOnly aria-describedby="profile-email-help" />
        </label>
        <small id="profile-email-help" className="field-help">Only an administrator can change your email.</small>
        <p className="field-help">Roles: {account.roles.length ? account.roles.map((role) => role.name).join(", ") : "none"}</p>
        {error && <p role="alert" className="form-error">{error}</p>}
        {notice && <p role="status" className="form-notice">{notice}</p>}
        <div><button className="primary-button" disabled={busy || name.trim() === account.user.display_name}>{busy && <LoaderCircle size={16} className="spin" />} Save name</button></div>
      </form>
    </section>
  );
}

function PasswordSection() {
  const { account, authorizedFetch } = useWorkspace();
  const { instance } = useSession();
  const [current, setCurrent] = useState("");
  const [password, setPassword] = useState("");
  const [confirmation, setConfirmation] = useState("");
  const [busy, setBusy] = useState(false);
  const [touched, setTouched] = useState(false);
  const [notice, setNotice] = useState<string | null>(null);
  const [error, setError] = useState<string | null>(null);
  const minLength = instance?.password_min_length ?? 12;
  const problem = newPasswordProblem(password, confirmation, minLength);

  if (!account.user.has_password) {
    return (
      <section className="management-panel">
        <div className="management-heading"><h2><KeyRound size={15} aria-hidden="true" /> Password</h2></div>
        <p className="field-help">Your account has no password. To set one, ask an administrator for a password reset link.</p>
      </section>
    );
  }

  const submit = async (event: FormEvent) => {
    event.preventDefault();
    setTouched(true);
    if (problem) return;
    setBusy(true);
    setError(null);
    setNotice(null);
    try {
      await expectOk(
        await authorizedFetch("/v1/account/password", jsonBody({ current_password: current, new_password: password })),
        "Your password could not be changed.",
      );
      setCurrent("");
      setPassword("");
      setConfirmation("");
      setTouched(false);
      setNotice("Your password was changed, and your other sessions were signed out.");
    } catch (failure) {
      if (!(failure instanceof AuthenticationRequired)) setError(errorText(failure, "Your password could not be changed."));
    } finally {
      setBusy(false);
    }
  };

  return (
    <section className="management-panel">
      <div className="management-heading"><h2><KeyRound size={15} aria-hidden="true" /> Password</h2><p>Changing it signs you out everywhere else.</p></div>
      <form className="form-stack form-narrow" onSubmit={(event) => void submit(event)}>
        <input type="email" autoComplete="username" value={account.user.email} readOnly hidden />
        <label htmlFor="current-password">Current password
          <input id="current-password" type="password" autoComplete="current-password" required value={current} onChange={(event) => setCurrent(event.target.value)} />
        </label>
        <NewPasswordFields label="New password" password={password} confirmation={confirmation} minLength={minLength} onPassword={setPassword} onConfirmation={setConfirmation} />
        {touched && problem && <p role="alert" className="form-error">{problem}</p>}
        {error && <p role="alert" className="form-error">{error}</p>}
        {notice && <p role="status" className="form-notice">{notice}</p>}
        <div><button className="primary-button" disabled={busy}>{busy && <LoaderCircle size={16} className="spin" />} Change password</button></div>
      </form>
    </section>
  );
}

type TotpSetup = { secret: string; otpauth_uri: string };

type Dialog = "start" | "disable" | "regenerate" | null;

// The second factors: an authenticator app and passkeys, each optional, and
// the recovery codes that stand in for them.
function TwoFactorSection() {
  const { account, authorizedFetch, refreshAccount, instanceName } = useWorkspace();
  const { two_factor: twoFactor } = account;
  const [dialog, setDialog] = useState<Dialog>(null);
  const [setup, setSetup] = useState<TotpSetup | null>(null);
  const [code, setCode] = useState("");
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  // Codes the server just issued; they are never shown again.
  const [codes, setCodes] = useState<string[] | null>(null);
  const [notice, setNotice] = useState<string | null>(null);
  // Removing the last second factor isn't allowed while the server requires one.
  const totpRequired = twoFactor.required && twoFactor.passkeys === 0;

  const showCodes = (issued: string[]) => {
    setCodes(issued);
    setNotice(null);
  };

  const startSetup = async (password: string) => {
    const response = await authorizedFetch("/v1/account/two-factor/totp", jsonBody({ password }));
    setSetup(await expectJson<TotpSetup>(response, "Two-factor setup could not start."));
    setCode("");
    setError(null);
    setNotice(null);
    setDialog(null);
  };

  const confirm = async (event: FormEvent) => {
    event.preventDefault();
    setBusy(true);
    setError(null);
    try {
      const response = await authorizedFetch("/v1/account/two-factor/totp/confirm", jsonBody({ code: code.replace(/\s/g, "") }));
      // No codes when a passkey already turned two-factor on: the user keeps theirs.
      const { recovery_codes } = await expectJson<{ recovery_codes: string[] | null }>(response, "The code could not be checked.");
      setSetup(null);
      if (recovery_codes) showCodes(recovery_codes);
      else setNotice("Your authenticator app is set up.");
      await refreshAccount().catch(() => {});
    } catch (failure) {
      if (!(failure instanceof AuthenticationRequired)) setError(errorText(failure, "The code could not be checked."));
    } finally {
      setBusy(false);
    }
  };

  const disable = async (password: string) => {
    await expectOk(await authorizedFetch("/v1/account/two-factor/totp/disable", jsonBody({ password })), "The authenticator app could not be removed.");
    setDialog(null);
    setCodes(null);
    setNotice(twoFactor.passkeys > 0 ? "Your authenticator app was removed." : "Your authenticator app was removed. Two-factor authentication is off.");
    await refreshAccount();
  };

  const regenerate = async (password: string) => {
    const response = await authorizedFetch("/v1/account/two-factor/recovery-codes", jsonBody({ password }));
    const { recovery_codes } = await expectJson<{ recovery_codes: string[] }>(response, "New recovery codes could not be made.");
    setDialog(null);
    showCodes(recovery_codes);
  };

  let body;
  if (codes) {
    body = (
      <>
        <p className="form-notice" role="status"><CircleCheck size={15} aria-hidden="true" /> Save these recovery codes somewhere safe. If you lose your phone or passkeys, each one signs you in once. They won&apos;t be shown again.</p>
        <RecoveryCodes codes={codes} instanceName={instanceName} email={account.user.email} />
        <div><button type="button" className="primary-button" onClick={() => setCodes(null)}>I&apos;ve saved my codes</button></div>
      </>
    );
  } else {
    let totp;
    if (setup) {
      totp = (
        <div className="totp-setup">
          <QrCode text={setup.otpauth_uri} label="QR code for your authenticator app" />
          <div>
            <ol className="totp-steps">
              <li>Open an authenticator app, such as 1Password, Google Authenticator or Microsoft Authenticator, and add an account.</li>
              <li>Scan the QR code, or enter this key: <code className="totp-secret">{setup.secret}</code></li>
              <li>Enter the 6-digit code the app shows.</li>
            </ol>
            <form className="form-stack" onSubmit={(event) => void confirm(event)}>
              <label htmlFor="totp-code">Code from the app
                <input id="totp-code" inputMode="numeric" autoComplete="one-time-code" required value={code} onChange={(event) => setCode(event.target.value)} />
              </label>
              {error && <p role="alert" className="form-error">{error}</p>}
              <div className="form-actions">
                <button className="primary-button" disabled={busy}>{busy && <LoaderCircle size={16} className="spin" />} Turn on</button>
                <button type="button" className="secondary-button" onClick={() => setSetup(null)} disabled={busy}>Cancel</button>
              </div>
            </form>
          </div>
        </div>
      );
    } else if (twoFactor.totp) {
      totp = (
        <>
          <p className="two-factor-status on"><CircleCheck size={16} aria-hidden="true" /> Set up.</p>
          {totpRequired
            ? <p className="field-help">This server requires two-factor authentication. Add a passkey before removing your authenticator app.</p>
            : <div><button type="button" className="danger-button" onClick={() => setDialog("disable")}><ShieldOff size={15} /> Remove authenticator app</button></div>}
        </>
      );
    } else {
      totp = (
        <>
          <div><button type="button" className={twoFactor.enabled ? "secondary-button" : "primary-button"} onClick={() => setDialog("start")} disabled={!account.user.has_password}><Smartphone size={15} /> Set up authenticator app</button></div>
          {!account.user.has_password && <p className="field-help">Set a password first: two-factor authentication protects password sign-in.</p>}
        </>
      );
    }

    body = (
      <>
        <p className={`two-factor-status ${twoFactor.enabled ? "on" : "off"}`}>{twoFactor.enabled ? <CircleCheck size={16} aria-hidden="true" /> : <CircleAlert size={16} aria-hidden="true" />} {twoFactorSummary(twoFactor)}</p>
        <div className="two-factor-methods">
          <div className="two-factor-method">
            <h3><Smartphone size={15} aria-hidden="true" /> Authenticator app</h3>
            <p className="field-help">A 6-digit code from an app on your phone.</p>
            {totp}
          </div>
          <PasskeysSection onRecoveryCodes={showCodes} onNotice={setNotice} />
          {twoFactor.enabled && (
            <div className="two-factor-method">
              <h3><KeyRound size={15} aria-hidden="true" /> Recovery codes</h3>
              <p className="field-help">Each signs you in once if you lose your phone and passkeys. {twoFactor.recovery_codes_remaining === 1 ? "1 code left." : `${twoFactor.recovery_codes_remaining} codes left.`}</p>
              {twoFactor.recovery_codes_remaining <= 3 && <p className="field-help field-problem">You&apos;re running out of recovery codes. Get new ones so you can still sign in without your phone or passkeys.</p>}
              <div><button type="button" className="secondary-button" onClick={() => setDialog("regenerate")}><RefreshCw size={15} /> New recovery codes</button></div>
            </div>
          )}
        </div>
      </>
    );
  }

  return (
    <section className="management-panel">
      <div className="management-heading"><h2><ShieldCheck size={15} aria-hidden="true" /> Two-factor authentication</h2><p>A code from your phone or a passkey at sign-in, so a stolen password isn&apos;t enough.</p></div>
      {notice && <p role="status" className="form-notice two-factor-notice">{notice}</p>}
      {body}
      {dialog === "start" && <PasswordPrompt title="Set up an authenticator app" description="Confirm it's you to add an authenticator app." action="Continue" onConfirm={startSetup} onClose={() => setDialog(null)} />}
      {dialog === "disable" && (
        <PasswordPrompt
          title="Remove your authenticator app"
          description={twoFactor.passkeys > 0
            ? "Its codes stop working. You keep signing in with a passkey as your second factor."
            : "Signing in will take only your password. Your recovery codes stop working."}
          action="Remove"
          danger
          onConfirm={disable}
          onClose={() => setDialog(null)}
        />
      )}
      {dialog === "regenerate" && <PasswordPrompt title="Get new recovery codes" description="Your current recovery codes stop working." action="Get new codes" onConfirm={regenerate} onClose={() => setDialog(null)} />}
    </section>
  );
}

type SessionView = {
  id: string;
  auth_method: string;
  created_at: number;
  last_seen_at: number;
  expires_at: number;
  ip: string | null;
  user_agent: string | null;
  current: boolean;
};

function SessionsSection() {
  const { authorizedFetch } = useWorkspace();
  const { signOut } = useSession();
  const load = useCallback(
    async () => expectJson<SessionView[]>(await authorizedFetch("/v1/account/sessions"), "Your sessions could not be loaded."),
    [authorizedFetch],
  );
  const { data: sessions, error: loadError, setData: setSessions } = useResource(load, "Your sessions could not be loaded.");
  const [actionError, setError] = useState<string | null>(null);
  const error = actionError ?? loadError;
  const [busyId, setBusyId] = useState<string | null>(null);

  const end = async (session: SessionView) => {
    if (session.current) {
      void signOut();
      return;
    }
    setBusyId(session.id);
    setError(null);
    try {
      await expectOk(await authorizedFetch(`/v1/account/sessions/${encodeURIComponent(session.id)}`, { method: "DELETE" }), "The session could not be signed out.");
      setSessions((current) => current.filter((candidate) => candidate.id !== session.id));
    } catch (failure) {
      if (!(failure instanceof AuthenticationRequired)) setError(errorText(failure, "The session could not be signed out."));
    } finally {
      setBusyId(null);
    }
  };

  return (
    <section className="management-panel">
      <div className="management-heading"><h2><MonitorSmartphone size={15} aria-hidden="true" /> Where you&apos;re signed in</h2><p>Sign out any browser you don&apos;t recognize, then change your password.</p></div>
      {error && <p role="alert" className="form-error">{error}</p>}
      {!sessions ? <p role="status" className="field-help"><LoaderCircle size={14} className="spin" /> Loading…</p> : (
        <ul className="session-list">
          {sessions.map((session) => (
            <li key={session.id}>
              <div>
                <strong>{describeUserAgent(session.user_agent)}{session.current && <span className="badge">This browser</span>}<span className="badge badge-muted" title="How this session signed in">{signInMethodLabel(session.auth_method)}</span></strong>
                <span>{session.ip ?? "Unknown address"} · active {formatRelative(session.last_seen_at)} · signed in {formatDateTime(session.created_at)}</span>
              </div>
              <button type="button" className="secondary-button" onClick={() => void end(session)} disabled={busyId === session.id}>
                {busyId === session.id ? <LoaderCircle size={15} className="spin" /> : <LogOut size={15} />} Sign out
              </button>
            </li>
          ))}
        </ul>
      )}
    </section>
  );
}
