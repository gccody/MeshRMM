import { KeyRound, LoaderCircle, LogIn, ShieldCheck } from "lucide-react";
import { type FormEvent, useState } from "react";
import { Link, Navigate, useNavigate, useSearchParams } from "react-router";
import { RequestError, apiFetch, errorText, expectJson, jsonBody } from "../../lib/http";
import { safeNextPath } from "./next-path";
import { AuthCard, FormError } from "./public-layout";
import { useDocumentTitle, useSession } from "./session";
import type { SignInResult } from "./types";

type Step = { kind: "password" } | { kind: "second-factor"; challenge: string; recovery: boolean };

// Email and password, then an authenticator or recovery code if the account
// has two-factor authentication.
export function LoginPage() {
  useDocumentTitle("Sign in");
  const { instance, state, signedIn } = useSession();
  const [params] = useSearchParams();
  const navigate = useNavigate();
  const next = safeNextPath(params.get("next"));
  const [step, setStep] = useState<Step>({ kind: "password" });
  const [email, setEmail] = useState("");
  const [password, setPassword] = useState("");
  const [code, setCode] = useState("");
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);

  if (instance?.setup_required) return <Navigate to="/setup" replace />;
  // Signing in here leaves busy set until the navigation below.
  if (state.status === "signed-in" && !busy) return <Navigate to={next} replace />;

  const finish = async (result: SignInResult) => {
    if (result.status === "second_factor_required") {
      setStep({ kind: "second-factor", challenge: result.challenge, recovery: false });
      setCode("");
      setBusy(false);
      return;
    }
    setPassword("");
    if (!(await signedIn())) {
      setError("You were signed in, but your account didn't load. Try again.");
      setBusy(false);
      return;
    }
    navigate(result.two_factor_enrollment_required ? "/account" : next, { replace: true });
  };

  const submitPassword = async (event: FormEvent) => {
    event.preventDefault();
    setBusy(true);
    setError(null);
    try {
      const response = await apiFetch("/v1/auth/sign-in", jsonBody({ email, password }));
      await finish(await expectJson<SignInResult>(response, "Sign-in failed. Try again."));
    } catch (failure) {
      setError(errorText(failure, "MeshRMM could not be reached. Try again."));
      setBusy(false);
    }
  };

  const submitCode = async (event: FormEvent) => {
    event.preventDefault();
    if (step.kind !== "second-factor") return;
    setBusy(true);
    setError(null);
    const body = step.recovery
      ? { challenge: step.challenge, recovery_code: code.trim() }
      : { challenge: step.challenge, code: code.replace(/\s/g, "") };
    try {
      const response = await apiFetch("/v1/auth/sign-in/second-factor", jsonBody(body));
      await finish(await expectJson<SignInResult>(response, "Sign-in failed. Try again."));
    } catch (failure) {
      // Too many wrong codes, or too slow: the password must be entered again.
      if (failure instanceof RequestError && failure.code === "challenge_expired") {
        setStep({ kind: "password" });
        setPassword("");
      }
      setError(errorText(failure, "MeshRMM could not be reached. Try again."));
      setBusy(false);
    }
  };

  if (step.kind === "second-factor") {
    return (
      <AuthCard icon={<ShieldCheck size={22} />} eyebrow={instance?.name} title="Two-factor authentication">
        <p>{step.recovery
          ? "Enter one of the recovery codes you saved when you set up two-factor authentication. Each code works once."
          : "Enter the 6-digit code from your authenticator app."}</p>
        <form className="form-stack" onSubmit={(event) => void submitCode(event)}>
          {step.recovery ? (
            <label htmlFor="recovery-code">Recovery code
              <input id="recovery-code" autoComplete="off" autoCapitalize="off" spellCheck={false} required value={code} onChange={(event) => setCode(event.target.value)} />
            </label>
          ) : (
            <label htmlFor="totp-code">Authentication code
              <input id="totp-code" inputMode="numeric" autoComplete="one-time-code" required value={code} onChange={(event) => setCode(event.target.value)} />
            </label>
          )}
          <FormError message={error} />
          <button className="primary-button" disabled={busy}>{busy ? <LoaderCircle size={16} className="spin" /> : <LogIn size={16} />} Sign in</button>
        </form>
        <div className="auth-links">
          <button type="button" className="link-button" onClick={() => { setStep({ ...step, recovery: !step.recovery }); setCode(""); setError(null); }}>
            {step.recovery ? "Use your authenticator app" : "Use a recovery code"}
          </button>
          <button type="button" className="link-button" onClick={() => { setStep({ kind: "password" }); setPassword(""); setError(null); }}>Start over</button>
        </div>
      </AuthCard>
    );
  }

  return (
    <AuthCard icon={<KeyRound size={22} />} eyebrow={instance?.name} title="Sign in">
      <form className="form-stack" onSubmit={(event) => void submitPassword(event)}>
        <label htmlFor="email">Email
          <input id="email" type="email" autoComplete="username" required value={email} onChange={(event) => setEmail(event.target.value)} />
        </label>
        <label htmlFor="password">Password
          <input id="password" type="password" autoComplete="current-password" required value={password} onChange={(event) => setPassword(event.target.value)} />
        </label>
        <FormError message={error} />
        <button className="primary-button" disabled={busy}>{busy ? <LoaderCircle size={16} className="spin" /> : <LogIn size={16} />} Sign in</button>
      </form>
      <div className="auth-links">
        <Link to="/reset">Forgot your password?</Link>
      </div>
    </AuthCard>
  );
}
