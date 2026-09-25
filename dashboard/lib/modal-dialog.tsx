"use client";

import { type MouseEvent, type ReactNode, type RefObject, useEffect, useEffectEvent, useRef } from "react";
import { focusTrapTarget, isDialogDismissal, tabbableElements } from "./focus-trap";

type Props = {
  className: string;
  // The id of the element that names the dialog.
  labelledBy: string;
  onClose: () => void;
  // The control that opened the dialog. Safari does not focus buttons on
  // click, so the focused element is only a fallback.
  returnFocus?: RefObject<HTMLElement | null>;
  children: ReactNode;
};

// A modal dialog over a backdrop. It takes focus when it opens, keeps Tab
// inside, closes on Escape or a backdrop click, and gives focus back to the
// control that opened it when it closes.
export function ModalDialog({ className, labelledBy, onClose, returnFocus, children }: Props) {
  const dialog = useRef<HTMLElement>(null);
  const close = useEffectEvent(onClose);

  useEffect(() => {
    const node = dialog.current;
    if (!node) return;
    const focused = document.activeElement;
    const opener = returnFocus?.current ?? (focused instanceof HTMLElement && focused !== document.body ? focused : null);
    node.focus();

    const onKeyDown = (event: KeyboardEvent) => {
      if (isDialogDismissal(event)) {
        event.preventDefault();
        close();
        return;
      }
      if (event.key !== "Tab") return;
      const tabbable = tabbableElements(node);
      const active = document.activeElement;
      const target = focusTrapTarget(
        tabbable.length,
        active instanceof HTMLElement ? tabbable.indexOf(active) : -1,
        event.shiftKey,
      );
      if (target === null) return;
      event.preventDefault();
      (target === -1 ? node : tabbable[target]).focus();
    };
    document.addEventListener("keydown", onKeyDown);
    return () => {
      document.removeEventListener("keydown", onKeyDown);
      // Unavailable openers (removed, disabled or hidden) ignore focus().
      if (opener?.isConnected) opener.focus();
    };
  }, [returnFocus]);

  // Preventing the default keeps the press from moving focus to the page after
  // the dialog has returned it to the opener.
  const closeFromBackdrop = (event: MouseEvent<HTMLDivElement>) => {
    if (event.target !== event.currentTarget) return;
    event.preventDefault();
    onClose();
  };

  return (
    <div className="modal-backdrop" role="presentation" onMouseDown={closeFromBackdrop}>
      <section ref={dialog} className={className} role="dialog" aria-modal="true" aria-labelledby={labelledBy} tabIndex={-1}>
        {children}
      </section>
    </div>
  );
}
