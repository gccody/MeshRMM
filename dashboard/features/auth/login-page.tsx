import { Fingerprint, LoaderCircle, LogIn } from "lucide-react";
import { type FormEvent, useState } from "react";
import { Link, Navigate, useNavigate, useSearchParams } from "react-router";
import { RequestError, apiFetch, errorText, expectJson, jsonBody } from "../../lib/http";
import { usePasskeySupport } from "../../lib/use-passkey-support";
import { type AssertionJSON, type PasskeyPrompt, PasskeyPromptError, type RequestOptionsJSON, getPasskey } from "../../lib/webauthn";
import { safeNextPath } from "./next-path";
import { AuthCard, FormError } from "./public-layout";
import { useDocumentTitle, useSession } from "./session";
import { type SecondFactorMethod, firstSecondFactor, ssoErrorMessage, ssoStartPath } from "./sign-in";
import type { SignInResult } from "./types";

type SecondFactor = {
  kind: "second-factor";
  challenge: string;
  methods: SecondFactorMethod[];
  // The prompt for the user's passkeys; one try spends it.
  passkey: RequestOptionsJSON | null;
  method: SecondFactorMethod;
};

type Step = { kind: "password" } | SecondFactor;

const METHOD_CHOICES: Record<SecondFactorMethod, string> = {
  totp: "Use your authenticator app",
  passkey: "Use a passkey",
  recovery_code: "Use a recovery code",
};

// Email and password, then an authenticator code, a passkey or a recovery
// code if the account has two-factor authentication. Single sign-on and
// passkeys sign in without a password, when the server offers them.
export function LoginPage() {
  useDocumentTitle("Sign in");
  const { instance, state, signedIn } = useSession();
  const [params] = useSearchParams();
  const navigate = useNavigate();
  const passkeySupport = usePasskeySupport();
  const next = safeNextPath(params.get("next"));
  const [step, setStep] = useState<Step>({ kind: "password" });
  const [email, setEmail] = useState("");
  const [password, setPassword] = useState("");
  const [code, setCode] = useState("");
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  // Why single sign-on sent the browser back, until the next attempt. The
  // prerendered page can't know the query, so it waits for the instance
  // like the sign-in buttons do.
  const [ssoErrorSeen, setSsoErrorSeen] = useState(false);
  const ssoError = instance && !ssoErrorSeen ? ssoErrorMessage(params.get("sso_error")) : null;

  if (instance?.setup_required) return <Navigate to="/setup" replace />;
  // Signing in here leaves busy set until the navigation below.
  if (state.status === "signed-in" && !busy) return <Navigate to={next} replace />;

  const passkeyUsable = (step: SecondFactor) => passkeySupport && step.passkey !== null && step.methods.includes("passkey");

  const finish = async (result: SignInResult) => {
    if (result.status === "second_factor_required") {
      const factor: SecondFactor = {
        kind: "second-factor",
        challenge: result.challenge,
        methods: result.methods,
        passkey: result.passkey ?? null,
        method: "totp",
      };
      setStep({ ...factor, method: firstSecondFactor(factor.methods, passkeyUsable(factor)) });
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

  const begin = () => {
    setBusy(true);
    setError(null);
    setSsoErrorSeen(true);
  };

  const submitPassword = async (event: FormEvent) => {
    event.preventDefault();
    begin();
    try {
      const response = await apiFetch("/v1/auth/sign-in", jsonBody({ email, password }));
      await finish(await expectJson<SignInResult>(response, "Sign-in failed. Try again."));
    } catch (failure) {
      setError(errorText(failure, "MeshRMM could not be reached. Try again."));
      setBusy(false);
    }
  };

  // A passkey alone signs in: the browser offers the passkeys it has for
  // this server, and the one chosen names the account.
  const signInWithPasskey = async () => {
    begin();
    try {
      const prompt = await expectJson<PasskeyPrompt<RequestOptionsJSON>>(
        await apiFetch("/v1/auth/passkey/options", { method: "POST" }),
        "Sign-in with a passkey couldn't start. Try again.",
      );
      const credential = await getPasskey(prompt.options);
      const response = await apiFetch("/v1/auth/passkey", jsonBody({ ceremony: prompt.ceremony, credential }));
      await finish(await expectJson<SignInResult>(response, "Sign-in failed. Try again."));
    } catch (failure) {
      if (!(failure instanceof PasskeyPromptError && failure.cancelled)) setError(errorText(failure, "MeshRMM could not be reached. Try again."));
      setBusy(false);
    }
  };

  const startOver = (message: string | null = null) => {
    setStep({ kind: "password" });
    setPassword("");
    setError(message);
  };

  const sendSecondFactor = async (factor: SecondFactor, body: Record<string, unknown>) => {
    try {
      const response = await apiFetch("/v1/auth/sign-in/second-factor", jsonBody({ challenge: factor.challenge, ...body }));
      await finish(await expectJson<SignInResult>(response, "Sign-in failed. Try again."));
    } catch (failure) {
      const message = errorText(failure, "MeshRMM could not be reached. Try again.");
      // Too many wrong codes, or too slow: the password must be entered again.
      if (failure instanceof RequestError && failure.code === "challenge_expired") startOver(message);
      else setError(message);
      setBusy(false);
    }
  };

  const submitCode = (event: FormEvent) => {
    event.preventDefault();
    if (step.kind !== "second-factor") return;
    begin();
    void sendSecondFactor(step, step.method === "recovery_code" ? { recovery_code: code.trim() } : { code: code.replace(/\s/g, "") });
  };

  const answerWithPasskey = async () => {
    if (step.kind !== "second-factor" || !step.passkey) return;
    begin();
    let credential: AssertionJSON;
    try {
      credential = await getPasskey(step.passkey);
    } catch (failure) {
      if (!(failure instanceof PasskeyPromptError && failure.cancelled)) setError(errorText(failure, "The passkey couldn't be used. Try again."));
      setBusy(false);
      return;
    }
    // The server checks one passkey per password sign-in, so another try
    // takes the password again; the other methods still work.
    const spent: SecondFactor = { ...step, passkey: null };
    setStep({ ...spent, method: spent.methods.includes("totp") ? "totp" : "recovery_code" });
    await sendSecondFactor(spent, { passkey: credential });
  };

  const choose = (method: SecondFactorMethod) => {
    if (step.kind !== "second-factor") return;
    setStep({ ...step, method });
    setCode("");
    setError(null);
  };

  if (step.kind === "second-factor") {
    return (
      <SecondFactorCard
        step={step} instanceName={instance?.name} passkeyUsable={passkeyUsable(step)} code={code} busy={busy} error={error}
        onCodeChange={setCode} onSubmitCode={submitCode} onPasskey={() => void answerWithPasskey()} onChoose={choose} onStartOver={() => startOver()}
      />
    );
  }

  return (
    <PasswordCard
      instanceName={instance?.name} ssoError={ssoError} sso={instance?.sign_in.sso ?? null} passkey={Boolean(instance?.sign_in.passkey) && passkeySupport} next={next}
      email={email} password={password} busy={busy} error={error} onEmailChange={setEmail} onPasswordChange={setPassword}
      onSubmit={(event) => void submitPassword(event)} onPasskey={() => void signInWithPasskey()}
    />
  );
}

function PasswordCard({ instanceName, ssoError, sso, passkey, next, email, password, busy, error, onEmailChange, onPasswordChange, onSubmit, onPasskey }: {
  instanceName: string | undefined;
  ssoError: string | null;
  sso: { name: string } | null;
  // Whether the server and this browser both offer passkey sign-in.
  passkey: boolean;
  next: string;
  email: string;
  password: string;
  busy: boolean;
  error: string | null;
  onEmailChange: (email: string) => void;
  onPasswordChange: (password: string) => void;
  onSubmit: (event: FormEvent) => void;
  onPasskey: () => void;
}) {
  return (
    <AuthCard eyebrow={instanceName} title="Sign in">
      {ssoError && <p className="form-error auth-sso-error" role="alert">{ssoError}</p>}
      <form className="form-stack" onSubmit={onSubmit}>
        <label htmlFor="email">Email
          <input id="email" type="email" autoComplete="username" required value={email} onChange={(event) => onEmailChange(event.target.value)} />
        </label>
        <label htmlFor="password">Password
          <input id="password" type="password" autoComplete="current-password" required value={password} onChange={(event) => onPasswordChange(event.target.value)} />
        </label>
        <FormError message={error} />
        <button className="primary-button" disabled={busy}>{busy ? <LoaderCircle size={16} className="spin" /> : <LogIn size={16} />} Sign in</button>
      </form>
      {(sso || passkey) && (
        <div className="auth-alternatives">
          <p className="auth-divider"><span>or</span></p>
          {passkey && <button type="button" className="secondary-button" disabled={busy} onClick={onPasskey}><Fingerprint size={16} /> Sign in with a passkey</button>}
          {sso && <a className="secondary-button" href={ssoStartPath(next)}><LogIn size={16} /> Sign in with {sso.name}</a>}
        </div>
      )}
      <div className="auth-links">
        <Link to="/reset">Forgot your password?</Link>
      </div>
    </AuthCard>
  );
}

function SecondFactorCard({ step, instanceName, passkeyUsable, code, busy, error, onCodeChange, onSubmitCode, onPasskey, onChoose, onStartOver }: {
  step: SecondFactor;
  instanceName: string | undefined;
  passkeyUsable: boolean;
  code: string;
  busy: boolean;
  error: string | null;
  onCodeChange: (code: string) => void;
  onSubmitCode: (event: FormEvent) => void;
  onPasskey: () => void;
  onChoose: (method: SecondFactorMethod) => void;
  onStartOver: () => void;
}) {
  const choices = (Object.keys(METHOD_CHOICES) as SecondFactorMethod[]).filter((method) =>
    method !== step.method && step.methods.includes(method) && (method !== "passkey" || passkeyUsable));
  return (
    <AuthCard eyebrow={instanceName} title="Two-factor authentication">
      {step.method === "passkey" ? (
        <>
          <p>Use one of your passkeys: on this device, your phone or a security key.</p>
          <div className="form-stack">
            <FormError message={error} />
            <button type="button" className="primary-button" disabled={busy} onClick={onPasskey}>{busy ? <LoaderCircle size={16} className="spin" /> : <Fingerprint size={16} />} Use a passkey</button>
          </div>
        </>
      ) : (
        <>
          <p>{step.method === "recovery_code"
            ? "Enter one of the recovery codes you saved when you set up two-factor authentication. Each code works once."
            : "Enter the 6-digit code from your authenticator app."}</p>
          <form className="form-stack" onSubmit={onSubmitCode}>
            {step.method === "recovery_code" ? (
              <label htmlFor="recovery-code">Recovery code
                <input id="recovery-code" autoComplete="off" autoCapitalize="off" spellCheck={false} required value={code} onChange={(event) => onCodeChange(event.target.value)} />
              </label>
            ) : (
              <label htmlFor="totp-code">Authentication code
                <input id="totp-code" inputMode="numeric" autoComplete="one-time-code" required value={code} onChange={(event) => onCodeChange(event.target.value)} />
              </label>
            )}
            <FormError message={error} />
            <button className="primary-button" disabled={busy}>{busy ? <LoaderCircle size={16} className="spin" /> : <LogIn size={16} />} Sign in</button>
          </form>
        </>
      )}
      <div className="auth-links">
        {choices.map((method) => <button key={method} type="button" className="link-button" disabled={busy} onClick={() => onChoose(method)}>{METHOD_CHOICES[method]}</button>)}
        <button type="button" className="link-button" disabled={busy} onClick={onStartOver}>Start over</button>
      </div>
    </AuthCard>
  );
}
