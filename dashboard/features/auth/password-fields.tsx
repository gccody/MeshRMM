import { useId } from "react";

export { newPasswordProblem } from "./passwords";

// A new password and its confirmation.
export function NewPasswordFields({ password, confirmation, minLength, onPassword, onConfirmation, label = "Password" }: {
  password: string;
  confirmation: string;
  minLength: number;
  onPassword: (value: string) => void;
  onConfirmation: (value: string) => void;
  label?: string;
}) {
  const id = useId();
  const mismatch = confirmation !== "" && password !== confirmation;
  return (
    <>
      <label htmlFor={`${id}-password`}>{label}
        <input id={`${id}-password`} type="password" autoComplete="new-password" required minLength={minLength} value={password} onChange={(event) => onPassword(event.target.value)} aria-describedby={`${id}-help`} />
      </label>
      <small id={`${id}-help`} className="field-help">At least {minLength} characters. A long phrase is easier to remember than a short, complex one.</small>
      <label htmlFor={`${id}-confirmation`}>Confirm {label.toLowerCase()}
        <input id={`${id}-confirmation`} type="password" autoComplete="new-password" required value={confirmation} onChange={(event) => onConfirmation(event.target.value)} aria-invalid={mismatch} />
      </label>
      {mismatch && <small className="field-help field-problem">The passwords don&apos;t match.</small>}
    </>
  );
}
