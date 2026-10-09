import { LoaderCircle, UserPlus } from "lucide-react";
import { type FormEvent, useEffect, useState } from "react";
import { Link, useNavigate } from "react-router";
import { apiFetch, errorText, expectJson, jsonBody } from "../../lib/http";
import { useHashToken } from "../../lib/use-hash-token";
import { NewPasswordFields, newPasswordProblem } from "./password-fields";
import { AuthCard, FormError, Pending } from "./public-layout";
import { useDocumentTitle, useSession } from "./session";
import type { SignInResult } from "./types";

type Invitation = { email: string; instance_name: string; expires_at: number };

type Lookup =
  | { status: "loading" }
  | { status: "found"; invitation: Invitation }
  | { status: "failed"; message: string };

// Accepting an invitation: the invited person picks their name and password.
export function InvitePage() {
  useDocumentTitle("Accept invitation");
  const { instance, account, signedIn } = useSession();
  const token = useHashToken();
  const navigate = useNavigate();
  const [lookup, setLookup] = useState<Lookup>({ status: "loading" });
  const [displayName, setDisplayName] = useState("");
  const [password, setPassword] = useState("");
  const [confirmation, setConfirmation] = useState("");
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [touched, setTouched] = useState(false);

  useEffect(() => {
    if (!token) return;
    let cancelled = false;
    apiFetch("/v1/auth/invitation", jsonBody({ token }))
      .then((response) => expectJson<Invitation>(response, "This invitation couldn't be checked."))
      .then((invitation) => { if (!cancelled) setLookup({ status: "found", invitation }); })
      .catch((failure: unknown) => {
        if (!cancelled) setLookup({ status: "failed", message: errorText(failure, "This invitation couldn't be checked.") });
      });
    return () => { cancelled = true; };
  }, [token]);

  if (token === null) {
    return (
      <AuthCard title="Invitation link incomplete">
        <p>Open the whole link from your invitation. If it was cut short, ask the person who invited you to send it again.</p>
      </AuthCard>
    );
  }
  if (token === undefined || lookup.status === "loading") {
    return <AuthCard title="Accept invitation"><Pending label="Checking your invitation…" /></AuthCard>;
  }
  if (lookup.status === "failed") {
    return (
      <AuthCard title="This invitation doesn't work">
        <p>{lookup.message} Ask the person who invited you for a new link.</p>
        <Link className="secondary-button" to="/login">Go to sign in</Link>
      </AuthCard>
    );
  }

  const { invitation } = lookup;
  const minLength = instance?.password_min_length ?? 12;
  const problem = newPasswordProblem(password, confirmation, minLength);

  const submit = async (event: FormEvent) => {
    event.preventDefault();
    setTouched(true);
    if (problem) return;
    setBusy(true);
    setError(null);
    try {
      const response = await apiFetch("/v1/auth/invitation/accept", jsonBody({ token, display_name: displayName.trim(), password }));
      const result = await expectJson<SignInResult>(response, "The invitation couldn't be accepted. Try again.");
      await signedIn();
      navigate(result.status === "signed_in" && result.two_factor_enrollment_required ? "/account" : "/", { replace: true });
    } catch (failure) {
      setError(errorText(failure, "MeshRMM could not be reached. Try again."));
      setBusy(false);
    }
  };

  return (
    <AuthCard eyebrow={invitation.instance_name} title="Create your account" wide>
      <p>You were invited as <strong>{invitation.email}</strong>. Choose your name and a password to finish.</p>
      {account && account.user.email !== invitation.email && (
        <p className="auth-note">This browser is signed in as {account.user.email}. Accepting signs it in as {invitation.email} instead.</p>
      )}
      <form className="form-stack" onSubmit={(event) => void submit(event)}>
        <label htmlFor="display-name">Your name
          <input id="display-name" autoComplete="name" required value={displayName} onChange={(event) => setDisplayName(event.target.value)} />
        </label>
        <NewPasswordFields password={password} confirmation={confirmation} minLength={minLength} onPassword={setPassword} onConfirmation={setConfirmation} />
        <FormError message={error ?? (touched ? problem : null)} />
        <button className="primary-button" disabled={busy}>{busy ? <LoaderCircle size={16} className="spin" /> : <UserPlus size={16} />} Create account</button>
      </form>
    </AuthCard>
  );
}
