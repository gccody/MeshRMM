"use client";

import type { FormEventHandler } from "react";
import { Check, Download, LoaderCircle, Monitor, ShieldCheck, X } from "lucide-react";

export type AgentPlatform = "windows-x64";

type Props = {
  companyName?: string;
  platform: AgentPlatform;
  error: string | null;
  isDownloading: boolean;
  downloaded: boolean;
  onClose: () => void;
  onPlatformChange: (platform: AgentPlatform) => void;
  onSubmit: FormEventHandler<HTMLFormElement>;
};

export function EnrollmentModal({
  companyName,
  platform,
  error,
  isDownloading,
  downloaded,
  onClose,
  onPlatformChange,
  onSubmit,
}: Props) {
  return (
    <div className="modal-backdrop" role="presentation" onMouseDown={(event) => event.target === event.currentTarget && onClose()}>
      <section className="settings-modal enrollment-modal" role="dialog" aria-modal="true" aria-labelledby="agent-title">
        <button className="modal-close" onClick={onClose} aria-label="Close"><X size={19} /></button>
        <div className="modal-icon"><Monitor size={22} /></div>
        <p className="eyebrow">{companyName}</p>
        <h2 id="agent-title">Download Agent installer</h2>
        <p>Select the endpoint platform and download one installer. Setup will use the Windows computer name automatically, generate the device ID on the server, configure the Agent, and install the LocalSystem service.</p>
        <form onSubmit={onSubmit}>
          <label>Installer platform<select required value={platform} onChange={(event) => onPlatformChange(event.target.value as AgentPlatform)}><option value="windows-x64">Windows 10/11 (x64)</option></select><small className="field-help">The Agent currently supports 64-bit Windows endpoints.</small></label>
          <div className="installer-summary">
            <div><Monitor size={18} /><span><strong>Automatic machine identity</strong><small>Computer name from Windows · server-generated device ID</small></span></div>
            <div><ShieldCheck size={18} /><span><strong>Administrator installation</strong><small>Automatic LocalSystem service with recovery</small></span></div>
          </div>
          {error && <div className="installer-error" role="alert">{error}</div>}
          <button className="primary-button modal-submit" disabled={isDownloading}>
            {isDownloading ? <LoaderCircle size={16} className="spin" /> : downloaded ? <Check size={16} /> : <Download size={16} />}
            {isDownloading ? "Preparing installer..." : downloaded ? "Download another installer" : "Download installer"}
          </button>
          <p className="installer-secret-note">Run the downloaded EXE and approve the Windows User Account Control prompt. Its enrollment authorization expires after 30 minutes and can only be used once.</p>
        </form>
      </section>
    </div>
  );
}

