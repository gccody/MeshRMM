"use client";

import { LogOut, ShieldCheck, X } from "lucide-react";
import type { RefObject } from "react";
import { ModalDialog } from "../../lib/modal-dialog";

type Props = {
  // The signed-in user's email, or null when nobody is signed in.
  email: string | null;
  displayName: string;
  initials: string;
  onClose: () => void;
  onSignIn: () => void;
  onSignOut: () => void;
  returnFocus?: RefObject<HTMLElement | null>;
};

export function AccountModal({ email, displayName, initials, onClose, onSignIn, onSignOut, returnFocus }: Props) {
  return (
    <ModalDialog className="settings-modal" labelledBy="account-title" onClose={onClose} returnFocus={returnFocus}>
      <button className="modal-close" onClick={onClose} aria-label="Close"><X size={19} /></button>
      <div className="modal-icon"><ShieldCheck size={22} /></div>
      <p className="eyebrow">Authenticated</p>
      <h2 id="account-title">Your account</h2>
      {email !== null ? <>
        <div className="account-summary"><div className="profile-avatar">{initials}</div><div><strong>{displayName}</strong><span>{email}</span></div></div>
        <button className="secondary-button modal-submit" onClick={onSignOut}><LogOut size={16} /> Sign out</button>
      </> : <button className="primary-button modal-submit" onClick={onSignIn}><ShieldCheck size={16} /> Sign in securely</button>}
    </ModalDialog>
  );
}
