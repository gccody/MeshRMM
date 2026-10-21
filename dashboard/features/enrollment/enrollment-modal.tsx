
import { type FormEventHandler, type RefObject, useState } from "react";
import { Check, Copy, Download, LoaderCircle, Terminal, X } from "lucide-react";
import { ModalDialog } from "../../lib/modal-dialog";
import type { Company } from "../agents/types";
import { type AgentPlatform, isWindowsPlatform } from "./installer";

const INTRODUCTIONS: Record<AgentPlatform, string> = {
  "windows-x64": "Run the installer on the computer. It appears here once set up.",
  "windows-arm64": "Run the installer on the computer. It appears here once set up.",
  macos: "Run an install command in Terminal on the Mac. It appears here once set up.",
  linux: "Run an install command in a terminal on the Linux computer. It appears here once set up.",
};

const NOTES: Record<AgentPlatform, string> = {
  "windows-x64": "Approve the Windows prompt when it runs. Works once, within 30 minutes.",
  "windows-arm64": "Approve the Windows prompt when it runs. Works once, within 30 minutes.",
  macos: "Then allow MeshRMM Agent in Privacy & Security when asked. Works once, within 30 minutes.",
  linux: "It installs the Agent as a systemd service. Works once, within 30 minutes.",
};

type Props = {
  platform: AgentPlatform;
  error: string | null;
  isDownloading: boolean;
  downloaded: boolean;
  /** The macOS or Linux install command, once one was created. */
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
  // macOS and Linux install with a terminal command rather than a download.
  const usesCommand = !isWindowsPlatform(platform);
  const copy = async () => {
    if (!command) return;
    await navigator.clipboard.writeText(command);
    setCopied(true);
  };
  return (
    <ModalDialog className="settings-modal enrollment-modal" labelledBy="agent-title" onClose={onClose} returnFocus={returnFocus}>
      <button className="modal-close" onClick={onClose} aria-label="Close"><X size={19} /></button>
      <h2 id="agent-title">Add a device</h2>
      <p>{INTRODUCTIONS[platform]}</p>
      <form onSubmit={onSubmit}>
        <label>Operating system<select required value={platform} onChange={(event) => { setCopied(false); onPlatformChange(event.target.value as AgentPlatform); }}><option value="windows-x64">Windows 10/11 (x64)</option><option value="windows-arm64">Windows 11 (ARM64)</option><option value="macos">macOS 12.3 or newer</option><option value="linux">Linux (x64 or arm64, systemd)</option></select></label>
        {companies.length > 0 && (
          <label>Company<select value={companyId} onChange={(event) => { setCopied(false); onCompanyChange(event.target.value); }}>
            <option value="">No company</option>
            {companies.map((company) => <option key={company.id} value={company.id}>{company.name}</option>)}
          </select></label>
        )}
        {error && <div className="installer-error" role="alert">{error}</div>}
        {usesCommand && command && (
          <div className="installer-command">
            <code>{command}</code>
            <button type="button" className="secondary-button" onClick={() => void copy()}>{copied ? <Check size={15} /> : <Copy size={15} />}{copied ? "Copied" : "Copy command"}</button>
          </div>
        )}
        <button className="primary-button modal-submit" disabled={isDownloading}>
          {isDownloading ? <LoaderCircle size={16} className="spin" /> : downloaded ? <Check size={16} /> : usesCommand ? <Terminal size={16} /> : <Download size={16} />}
          {usesCommand
            ? isDownloading ? "Creating command..." : downloaded ? "Create another command" : "Create install command"
            : isDownloading ? "Preparing installer..." : downloaded ? "Download another installer" : "Download installer"}
        </button>
        <p className="installer-secret-note">{NOTES[platform]}</p>
      </form>
    </ModalDialog>
  );
}
