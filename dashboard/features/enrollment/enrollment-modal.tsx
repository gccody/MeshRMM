"use client";

import type { FormEventHandler, RefObject } from "react";
import { Check, Download, LoaderCircle, Monitor, ShieldCheck, X } from "lucide-react";
import { ModalDialog } from "../../lib/modal-dialog";
import type { AgentPlatform } from "./installer";

type Props = {
  companyName?: string;
  platform: AgentPlatform;
  error: string | null;
  isDownloading: boolean;
  downloaded: boolean;
  onClose: () => void;
  onPlatformChange: (platform: AgentPlatform) => void;
  onSubmit: FormEventHandler<HTMLFormElement>;
  returnFocus?: RefObject<HTMLElement | null>;
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
  returnFocus,
}: Props) {
  return (
    <ModalDialog className="settings-modal enrollment-modal" labelledBy="agent-title" onClose={onClose} returnFocus={returnFocus}>
      <button className="modal-close" onClick={onClose} aria-label="Close"><X size={19} /></button>
      <div className="modal-icon"><Monitor size={22} /></div>
      <p className="eyebrow">{companyName}</p>
      <h2 id="agent-title">Add a device</h2>
      <p>Download the installer and run it on the device you want to manage. The device will appear in this workspace after setup.</p>
      <form onSubmit={onSubmit}>
        <label>Operating system<select required value={platform} onChange={(event) => onPlatformChange(event.target.value as AgentPlatform)}><option value="windows-x64">Windows 10/11 (x64)</option></select><small className="field-help">Supports 64-bit Windows 10 and 11.</small></label>
        <div className="installer-summary">
          <div><Monitor size={18} /><span><strong>Easy to find</strong><small>Appears using its Windows computer name</small></span></div>
          <div><ShieldCheck size={18} /><span><strong>Administrator installation</strong><small>Runs automatically when the computer starts</small></span></div>
        </div>
        {error && <div className="installer-error" role="alert">{error}</div>}
        <button className="primary-button modal-submit" disabled={isDownloading}>
          {isDownloading ? <LoaderCircle size={16} className="spin" /> : downloaded ? <Check size={16} /> : <Download size={16} />}
          {isDownloading ? "Preparing installer..." : downloaded ? "Download another installer" : "Download installer"}
        </button>
        <p className="installer-secret-note">Run the downloaded EXE and approve the Windows User Account Control prompt. Its enrollment authorization expires after 30 minutes and can only be used once.</p>
      </form>
    </ModalDialog>
  );
}

