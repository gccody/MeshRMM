
import { type FormEventHandler, type RefObject, useState } from "react";
import { Check, Copy, Download, LoaderCircle, Monitor, ShieldCheck, Terminal, X } from "lucide-react";
import { ModalDialog } from "../../lib/modal-dialog";
import type { AgentPlatform } from "./installer";

type Props = {
  instanceName: string;
  platform: AgentPlatform;
  error: string | null;
  isDownloading: boolean;
  downloaded: boolean;
  /** The macOS install command, once one was created. */
  command: string | null;
  onClose: () => void;
  onPlatformChange: (platform: AgentPlatform) => void;
  onSubmit: FormEventHandler<HTMLFormElement>;
  returnFocus?: RefObject<HTMLElement | null>;
};

export function EnrollmentModal({
  instanceName,
  platform,
  error,
  isDownloading,
  downloaded,
  command,
  onClose,
  onPlatformChange,
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
      <div className="modal-icon"><Monitor size={22} /></div>
      <p className="eyebrow">{instanceName}</p>
      <h2 id="agent-title">Add a device</h2>
      <p>{mac
        ? "Create an install command and run it in Terminal on the Mac you want to manage. The Mac will appear here after setup."
        : "Download the installer and run it on the device you want to manage. The device will appear here after setup."}</p>
      <form onSubmit={onSubmit}>
        <label>Operating system<select required value={platform} onChange={(event) => { setCopied(false); onPlatformChange(event.target.value as AgentPlatform); }}><option value="windows-x64">Windows 10/11 (x64)</option><option value="macos">macOS 12.3 or newer</option></select><small className="field-help">{mac ? "Supports Apple silicon and Intel Macs." : "Supports 64-bit Windows 10 and 11."}</small></label>
        <div className="installer-summary">
          <div><Monitor size={18} /><span><strong>Easy to find</strong><small>{mac ? "Appears using its Mac computer name" : "Appears using its Windows computer name"}</small></span></div>
          <div><ShieldCheck size={18} /><span><strong>Administrator installation</strong><small>Runs automatically when the computer starts</small></span></div>
        </div>
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
          ? "Paste the command into Terminal and enter an administrator password. Then allow MeshRMM Agent in System Settings > Privacy & Security when the Mac asks. The command works once and expires after 30 minutes."
          : "Run the downloaded EXE and approve the Windows User Account Control prompt. Its enrollment authorization expires after 30 minutes and can only be used once."}</p>
      </form>
    </ModalDialog>
  );
}
