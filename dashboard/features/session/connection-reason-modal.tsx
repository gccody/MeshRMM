
import { type FormEvent, useState } from "react";
import { Monitor, X } from "lucide-react";
import { ModalDialog } from "../../lib/modal-dialog";
import { MAX_CONNECTION_REASON_BYTES, isConnectionReasonValid } from "./connection-reason";

type Props = {
  agentName: string;
  background: boolean;
  onCancel: () => void;
  onConnect: (reason: string) => void;
};

// Asks for an optional reason before connecting to a device whose user must
// approve the connection. The reason appears in the approval prompt.
export function ConnectionReasonModal({ agentName, background, onCancel, onConnect }: Props) {
  const [reason, setReason] = useState("");
  const valid = isConnectionReasonValid(reason);

  const submit = (event: FormEvent) => {
    event.preventDefault();
    if (valid) onConnect(reason.trim());
  };

  return (
    <ModalDialog className="settings-modal connection-reason-modal" labelledBy="connection-reason-title" onClose={onCancel}>
      <button type="button" className="modal-close" onClick={onCancel} aria-label="Close"><X size={19} /></button>
      <div className="modal-icon"><Monitor size={22} /></div>
      <p className="eyebrow">{background ? "Connect to background" : "Connect"}</p>
      <h2 id="connection-reason-title">Request access to {agentName}</h2>
      <p>The person at this device is asked to accept the connection. A reason helps them decide.</p>
      <form onSubmit={submit}>
        <label htmlFor="connection-reason">Reason (optional)<textarea id="connection-reason" rows={3} maxLength={MAX_CONNECTION_REASON_BYTES} value={reason} onChange={(event) => setReason(event.target.value)} aria-describedby="connection-reason-help" aria-invalid={!valid} /></label>
        <small id="connection-reason-help" className="field-help">Without a reason, the prompt only asks to accept your connection. Up to {MAX_CONNECTION_REASON_BYTES} bytes.</small>
        {!valid && <p role="alert" className="installer-error">Shorten the reason to {MAX_CONNECTION_REASON_BYTES} bytes and remove tabs or other control characters.</p>}
        <div className="connection-reason-actions">
          <button type="button" className="secondary-button" onClick={onCancel}>Cancel</button>
          <button className="primary-button" disabled={!valid}><Monitor size={16} /> Request connection</button>
        </div>
      </form>
    </ModalDialog>
  );
}
