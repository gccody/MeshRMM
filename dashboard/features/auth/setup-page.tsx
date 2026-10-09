import { LoaderCircle, Rocket } from "lucide-react";
import { type FormEvent, useState } from "react";
import { Link, useNavigate } from "react-router";
import { RequestError, apiFetch, errorText, expectJson, jsonBody } from "../../lib/http";
import { useHashToken } from "../../lib/use-hash-token";
import { NewPasswordFields, newPasswordProblem } from "./password-fields";
import { AuthCard, FormError, Pending } from "./public-layout";
import { useDocumentTitle, useSession } from "./session";
import type { SignInResult } from "./types";

// The server's limit on the instance name.
export const MAX_INSTANCE_NAME_LENGTH = 120;

// First run: whoever holds the link from the server log names the server and
// creates the first administrator.
export function SetupPage() {
  useDocumentTitle("First-run setup");
  const { instance, state, signedIn } = useSession();
  const token = useHashToken();
  const navigate = useNavigate();
  const [instanceName, setInstanceName] = useState("");
  const [displayName, setDisplayName] = useState("");
  const [email, setEmail] = useState("");
  const [password, setPassword] = useState("");
  const [confirmation, setConfirmation] = useState("");
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [touched, setTouched] = useState(false);

  if (token === undefined || (!instance && state.status === "loading")) {
    return <AuthCard title="Set up MeshRMM"><Pending label="Checking this server…" /></AuthCard>;
  }

  if (instance && !instance.setup_required && !busy) {
    return (
      <AuthCard eyebrow={instance.name} title="This server is set up">
        <p>Its first administrator already exists. Sign in, or ask an administrator to invite you.</p>
        <Link className="primary-button" to="/login">Sign in</Link>
      </AuthCard>
    );
  }

  if (!token) {
    return (
      <AuthCard title="Open the setup link">
        <p>
          When it has no accounts yet, the server writes a one-time setup link to its log. Open that link to create the
          first administrator. It changes every time the server restarts, so use the newest one.
        </p>
        <pre className="auth-command">journalctl -u meshrmm-server | grep setup</pre>
        <p className="field-help">With Docker, run <code>docker logs</code> on the container instead.</p>
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
      const response = await apiFetch("/v1/setup", jsonBody({
        token,
        instance_name: instanceName.trim(),
        email: email.trim(),
        display_name: displayName.trim(),
        password,
      }));
      const result = await expectJson<SignInResult>(response, "Setup failed. Try again.");
      await signedIn();
      navigate(result.status === "signed_in" && result.two_factor_enrollment_required ? "/account" : "/", { replace: true });
    } catch (failure) {
      setError(failure instanceof RequestError && failure.code === "setup_complete"
        ? "Someone already finished setup. Sign in instead."
        : errorText(failure, "MeshRMM could not be reached. Try again."));
      setBusy(false);
    }
  };

  return (
    <AuthCard eyebrow="First run" title="Set up MeshRMM" wide>
      <p>Name this server and create your administrator account. You can invite your team once you&apos;re in.</p>
      <form className="form-stack" onSubmit={(event) => void submit(event)}>
        <label htmlFor="instance-name">Server name
          <input id="instance-name" required maxLength={MAX_INSTANCE_NAME_LENGTH} placeholder="Acme IT" value={instanceName} onChange={(event) => setInstanceName(event.target.value)} aria-describedby="instance-name-help" />
        </label>
        <small id="instance-name-help" className="field-help">Shown in the website and in authenticator apps. Usually your company&apos;s name.</small>
        <label htmlFor="display-name">Your name
          <input id="display-name" autoComplete="name" required value={displayName} onChange={(event) => setDisplayName(event.target.value)} />
        </label>
        <label htmlFor="email">Email
          <input id="email" type="email" autoComplete="username" required value={email} onChange={(event) => setEmail(event.target.value)} />
        </label>
        <NewPasswordFields password={password} confirmation={confirmation} minLength={minLength} onPassword={setPassword} onConfirmation={setConfirmation} />
        <FormError message={error ?? (touched ? problem : null)} />
        <button className="primary-button" disabled={busy}>{busy ? <LoaderCircle size={16} className="spin" /> : <Rocket size={16} />} Create administrator</button>
      </form>
    </AuthCard>
  );
}
