
import { type FormEventHandler, type RefObject, useState } from "react";
import { Check, Copy, Download, LoaderCircle, Terminal, X } from "lucide-react";
import { ModalDialog } from "../../lib/modal-dialog";
import type { Company } from "../agents/types";
import type { AgentPlatform } from "./installer";

type Props = {
  platform: AgentPlatform;
  error: string | null;
  isDownloading: boolean;
  downloaded: boolean;
  /** The macOS install command, once one was created. */
  command: string | null;
  onClose: () => void;
  onPlatformChange: (platform: AgentPlatform) => void;
  companies: Company[];
  /** The company the device joins; "" for none. */
  companyId: string;
  onCompanyChange: (companyId: string) => void;
  onSubmit: FormEventHandler<HTMLFormElement>;
  returnFocus?: RefObject<HTMLElement | null>;
};

export function EnrollmentModal({
  platform,
  error,
  isDownloading,
  downloaded,
  command,
  onClose,
  onPlatformChange,
  companies,
  companyId,
  onCompanyChange,
  onSubmit,
  returnFocus,
}: Props) {
  const [copied, setCopied] = useState(false);
  const mac = platform === "macos";
  const copy = async () => {
    if (!command) return;
    await navigator.clipboard.writeText(command);
    setCopied(true);
  };
  return (
    <ModalDialog className="settings-modal enrollment-modal" labelledBy="agent-title" onClose={onClose} returnFocus={returnFocus}>
      <button className="modal-close" onClick={onClose} aria-label="Close"><X size={19} /></button>
      <h2 id="agent-title">Add a device</h2>
      <p>{mac
        ? "Run an install command in Terminal on the Mac. It appears here once set up."
        : "Run the installer on the computer. It appears here once set up."}</p>
      <form onSubmit={onSubmit}>
        <label>Operating system<select required value={platform} onChange={(event) => { setCopied(false); onPlatformChange(event.target.value as AgentPlatform); }}><option value="windows-x64">Windows 10/11 (x64)</option><option value="macos">macOS 12.3 or newer</option></select></label>
        {companies.length > 0 && (
          <label>Company<select value={companyId} onChange={(event) => { setCopied(false); onCompanyChange(event.target.value); }}>
            <option value="">No company</option>
            {companies.map((company) => <option key={company.id} value={company.id}>{company.name}</option>)}
          </select></label>
        )}
        {error && <div className="installer-error" role="alert">{error}</div>}
        {mac && command && (
          <div className="installer-command">
            <code>{command}</code>
            <button type="button" className="secondary-button" onClick={() => void copy()}>{copied ? <Check size={15} /> : <Copy size={15} />}{copied ? "Copied" : "Copy command"}</button>
          </div>
        )}
        <button className="primary-button modal-submit" disabled={isDownloading}>
          {isDownloading ? <LoaderCircle size={16} className="spin" /> : downloaded ? <Check size={16} /> : mac ? <Terminal size={16} /> : <Download size={16} />}
          {mac
            ? isDownloading ? "Creating command..." : downloaded ? "Create another command" : "Create install command"
            : isDownloading ? "Preparing installer..." : downloaded ? "Download another installer" : "Download installer"}
        </button>
        <p className="installer-secret-note">{mac
          ? "Then allow MeshRMM Agent in Privacy & Security when asked. Works once, within 30 minutes."
          : "Approve the Windows prompt when it runs. Works once, within 30 minutes."}</p>
      </form>
    </ModalDialog>
  );
}
