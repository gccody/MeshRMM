import { CircleCheck, KeyRound, LoaderCircle, Mail } from "lucide-react";
import { type FormEvent, useEffect, useState } from "react";
import { Link } from "react-router";
import { apiFetch, errorText, expectJson, expectOk, jsonBody } from "../../lib/http";
import { useHashToken } from "../../lib/use-hash-token";
import { NewPasswordFields, newPasswordProblem } from "./password-fields";
import { AuthCard, FormError, Pending } from "./public-layout";
import { useDocumentTitle, useSession } from "./session";

// A reset link sets a new password; without one, the page asks for a link
// by email when the server can send it.
export function ResetPage() {
  useDocumentTitle("Reset password");
  const token = useHashToken();
  if (token === undefined) return <AuthCard icon={<KeyRound size={22} />} title="Reset your password"><Pending label="Loading…" /></AuthCard>;
  return token ? <SetNewPassword token={token} /> : <RequestReset />;
}

function RequestReset() {
  const { instance } = useSession();
  const [email, setEmail] = useState("");
  const [busy, setBusy] = useState(false);
  const [sent, setSent] = useState(false);
  const [error, setError] = useState<string | null>(null);

  if (!instance) return <AuthCard icon={<KeyRound size={22} />} title="Reset your password"><Pending label="Loading…" /></AuthCard>;

  if (!instance.sign_in.password_reset_email) {
    return (
      <AuthCard icon={<KeyRound size={22} />} eyebrow={instance.name} title="Reset your password">
        <p>This server doesn&apos;t send email. Ask an administrator for a password reset link; they can make one on the Users page.</p>
        <Link className="secondary-button" to="/login">Back to sign in</Link>
      </AuthCard>
    );
  }

  if (sent) {
    return (
      <AuthCard icon={<Mail size={22} />} eyebrow={instance.name} title="Check your email">
        <p>If an account uses {email.trim()}, a link to reset its password is on its way. The link works for an hour.</p>
        <Link className="secondary-button" to="/login">Back to sign in</Link>
      </AuthCard>
    );
  }

  const submit = async (event: FormEvent) => {
    event.preventDefault();
    setBusy(true);
    setError(null);
    try {
      await expectOk(await apiFetch("/v1/auth/password-reset", jsonBody({ email: email.trim() })), "The link couldn't be sent. Try again.");
      setSent(true);
    } catch (failure) {
      setError(errorText(failure, "MeshRMM could not be reached. Try again."));
    } finally {
      setBusy(false);
    }
  };

  return (
    <AuthCard icon={<KeyRound size={22} />} eyebrow={instance.name} title="Reset your password">
      <p>Enter your account&apos;s email and we&apos;ll send you a link to choose a new password.</p>
      <form className="form-stack" onSubmit={(event) => void submit(event)}>
        <label htmlFor="email">Email
          <input id="email" type="email" autoComplete="username" required value={email} onChange={(event) => setEmail(event.target.value)} />
        </label>
        <FormError message={error} />
        <button className="primary-button" disabled={busy}>{busy ? <LoaderCircle size={16} className="spin" /> : <Mail size={16} />} Send reset link</button>
      </form>
      <div className="auth-links"><Link to="/login">Back to sign in</Link></div>
    </AuthCard>
  );
}

type Lookup = { status: "loading" } | { status: "found"; email: string } | { status: "failed"; message: string };

function SetNewPassword({ token }: { token: string }) {
  const { instance, refresh } = useSession();
  const [lookup, setLookup] = useState<Lookup>({ status: "loading" });
  const [password, setPassword] = useState("");
  const [confirmation, setConfirmation] = useState("");
  const [busy, setBusy] = useState(false);
  const [done, setDone] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [touched, setTouched] = useState(false);

  useEffect(() => {
    let cancelled = false;
    apiFetch("/v1/auth/password-reset/lookup", jsonBody({ token }))
      .then((response) => expectJson<{ email: string }>(response, "This link couldn't be checked."))
      .then(({ email }) => { if (!cancelled) setLookup({ status: "found", email }); })
      .catch((failure: unknown) => {
        if (!cancelled) setLookup({ status: "failed", message: errorText(failure, "This link couldn't be checked.") });
      });
    return () => { cancelled = true; };
  }, [token]);

  if (lookup.status === "loading") return <AuthCard icon={<KeyRound size={22} />} title="Reset your password"><Pending label="Checking your link…" /></AuthCard>;
  if (lookup.status === "failed") {
    return (
      <AuthCard icon={<KeyRound size={22} />} title="This link doesn't work">
        <p>{lookup.message} Reset links work once, for a limited time.</p>
        <div className="auth-actions">
          <Link className="primary-button" to="/reset">Get a new link</Link>
          <Link className="secondary-button" to="/login">Back to sign in</Link>
        </div>
      </AuthCard>
    );
  }
  if (done) {
    return (
      <AuthCard icon={<CircleCheck size={22} />} title="Password changed">
        <p>Every session of {lookup.email} was signed out. Sign in with your new password.</p>
        <Link className="primary-button" to="/login">Sign in</Link>
      </AuthCard>
    );
  }

  const minLength = instance?.password_min_length ?? 12;
  const problem = newPasswordProblem(password, confirmation, minLength);

  const submit = async (event: FormEvent) => {
    event.preventDefault();
    setTouched(true);
    if (problem) return;
    setBusy(true);
    setError(null);
    try {
      await expectOk(await apiFetch("/v1/auth/password-reset/complete", jsonBody({ token, password })), "The password couldn't be changed. Try again.");
      setDone(true);
      // This browser's own session may have been one of those signed out.
      void refresh().catch(() => {});
    } catch (failure) {
      setError(errorText(failure, "MeshRMM could not be reached. Try again."));
    } finally {
      setBusy(false);
    }
  };

  return (
    <AuthCard icon={<KeyRound size={22} />} eyebrow={instance?.name} title="Choose a new password">
      <p>For {lookup.email}. Every place the account is signed in will be signed out.</p>
      <form className="form-stack" onSubmit={(event) => void submit(event)}>
        <input type="email" autoComplete="username" value={lookup.email} readOnly hidden />
        <NewPasswordFields label="New password" password={password} confirmation={confirmation} minLength={minLength} onPassword={setPassword} onConfirmation={setConfirmation} />
        <FormError message={error ?? (touched ? problem : null)} />
        <button className="primary-button" disabled={busy}>{busy ? <LoaderCircle size={16} className="spin" /> : <KeyRound size={16} />} Change password</button>
      </form>
    </AuthCard>
  );
}
