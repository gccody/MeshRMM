import { Check, Copy, Download, LoaderCircle, X } from "lucide-react";
import { type FormEvent, type ReactNode, useId, useState } from "react";
import { AuthenticationRequired, errorText } from "../../lib/http";
import { ModalDialog } from "../../lib/modal-dialog";

// Asks for the user's password before a security change.
export function PasswordPrompt({ title, description, action, danger = false, onConfirm, onClose }: {
  title: string;
  description: ReactNode;
  action: string;
  danger?: boolean;
  // Resolves when the change is done; a rejection's message is shown.
  onConfirm: (password: string) => Promise<void>;
  onClose: () => void;
}) {
  const id = useId();
  const [password, setPassword] = useState("");
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);

  const submit = async (event: FormEvent) => {
    event.preventDefault();
    setBusy(true);
    setError(null);
    try {
      await onConfirm(password);
    } catch (failure) {
      if (failure instanceof AuthenticationRequired) return;
      setError(errorText(failure, "That didn't work. Try again."));
      setBusy(false);
    }
  };

  return (
    <ModalDialog className="settings-modal" labelledBy={`${id}-title`} onClose={busy ? () => {} : onClose}>
      <button type="button" className="modal-close" onClick={onClose} aria-label="Close" disabled={busy}><X size={19} /></button>
      <h2 id={`${id}-title`}>{title}</h2>
      <p>{description}</p>
      <form onSubmit={(event) => void submit(event)}>
        <label htmlFor={`${id}-password`}>Your password
          <input id={`${id}-password`} type="password" autoComplete="current-password" required value={password} onChange={(event) => setPassword(event.target.value)} />
        </label>
        {error && <p role="alert" className="installer-error">{error}</p>}
        <div className="connection-reason-actions">
          <button type="button" className="secondary-button" onClick={onClose} disabled={busy}>Cancel</button>
          <button className={danger ? "danger-button" : "primary-button"} disabled={busy}>{busy && <LoaderCircle size={16} className="spin" />} {action}</button>
        </div>
      </form>
    </ModalDialog>
  );
}

// Recovery codes, shown once: each signs in once in place of an
// authenticator code.
export function RecoveryCodes({ codes, instanceName, email }: { codes: string[]; instanceName: string; email: string }) {
  const [copied, setCopied] = useState(false);
  const text = `${instanceName} recovery codes for ${email}\nEach code works once.\n\n${codes.join("\n")}\n`;
  const copy = async () => {
    await navigator.clipboard.writeText(text);
    setCopied(true);
  };
  const download = () => {
    const url = URL.createObjectURL(new Blob([text], { type: "text/plain" }));
    const link = document.createElement("a");
    link.href = url;
    link.download = "meshrmm-recovery-codes.txt";
    document.body.appendChild(link);
    link.click();
    link.remove();
    window.setTimeout(() => URL.revokeObjectURL(url), 1_000);
  };
  return (
    <div className="recovery-codes">
      <ol aria-label="Recovery codes">
        {codes.map((code) => <li key={code}><code>{code}</code></li>)}
      </ol>
      <div className="recovery-code-actions">
        <button type="button" className="secondary-button" onClick={() => void copy()}>{copied ? <Check size={15} /> : <Copy size={15} />}{copied ? "Copied" : "Copy"}</button>
        <button type="button" className="secondary-button" onClick={download}><Download size={15} /> Download</button>
      </div>
    </div>
  );
}
