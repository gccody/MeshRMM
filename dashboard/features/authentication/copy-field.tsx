import { Check, Copy } from "lucide-react";
import { useState } from "react";

// A value to paste into another system, under its name, with a button that
// copies it.
export function CopyField({ label, value }: { label: string; value: string }) {
  const [copied, setCopied] = useState(false);
  const copy = async () => {
    await navigator.clipboard.writeText(value);
    setCopied(true);
  };
  return (
    <div className="copy-field">
      <span>{label}</span>
      <div className="installer-command">
        <code>{value}</code>
        <button type="button" className="secondary-button" onClick={() => void copy()} aria-label={copied ? `${label} copied` : `Copy ${label}`}>{copied ? <Check size={15} /> : <Copy size={15} />}{copied ? "Copied" : "Copy"}</button>
      </div>
    </div>
  );
}
