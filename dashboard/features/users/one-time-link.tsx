import { Check, CircleAlert, Copy, Mail } from "lucide-react";
import { useState } from "react";
import { formatDateTime } from "../../lib/format";
import type { Delivery } from "./model";

// Where a one-time link went: the recipient's inbox, or this screen, for
// the administrator to pass on.
export function OneTimeLink({ delivery, recipient, expiresAt }: { delivery: Delivery; recipient: string; expiresAt: number }) {
  const [copied, setCopied] = useState(false);
  if (delivery.emailed) {
    return <p className="form-notice" role="status"><Mail size={15} aria-hidden="true" /> Emailed to {recipient}. The link works until {formatDateTime(expiresAt)}.</p>;
  }
  const copy = async () => {
    if (!delivery.link) return;
    await navigator.clipboard.writeText(delivery.link);
    setCopied(true);
  };
  return (
    <div className="one-time-link">
      {delivery.email_error && <p className="form-error" role="alert"><CircleAlert size={15} aria-hidden="true" /> {delivery.email_error} Send this link to {recipient} yourself instead.</p>}
      {!delivery.email_error && <p className="field-help">This server doesn&apos;t send email. Send this link to {recipient} yourself, by a channel only they can read. It works once, until {formatDateTime(expiresAt)}.</p>}
      <div className="installer-command">
        <code>{delivery.link}</code>
        <button type="button" className="secondary-button" onClick={() => void copy()}>{copied ? <Check size={15} /> : <Copy size={15} />}{copied ? "Copied" : "Copy link"}</button>
      </div>
    </div>
  );
}
