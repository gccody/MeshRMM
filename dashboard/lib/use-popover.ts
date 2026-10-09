import { useEffect, useRef, useState } from "react";

// A menu or panel that opens from a trigger button and closes on a click
// outside it or on Escape, which gives focus back to the trigger.
export function usePopover<Trigger extends HTMLElement = HTMLButtonElement>() {
  const [open, setOpen] = useState(false);
  const container = useRef<HTMLDivElement>(null);
  const trigger = useRef<Trigger>(null);

  useEffect(() => {
    if (!open) return;
    const closeOnOutsideClick = (event: PointerEvent) => {
      if (!container.current?.contains(event.target as Node)) setOpen(false);
    };
    const closeOnEscape = (event: KeyboardEvent) => {
      if (event.key !== "Escape") return;
      setOpen(false);
      trigger.current?.focus();
    };
    document.addEventListener("pointerdown", closeOnOutsideClick);
    document.addEventListener("keydown", closeOnEscape);
    return () => {
      document.removeEventListener("pointerdown", closeOnOutsideClick);
      document.removeEventListener("keydown", closeOnEscape);
    };
  }, [open]);

  // Closes the popover and returns focus to its trigger, for a chosen item.
  const close = () => {
    setOpen(false);
    trigger.current?.focus();
  };

  return { open, setOpen, close, container, trigger };
}
