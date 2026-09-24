// Keyboard handling for modal dialogs: Escape closes, and Tab cycles through
// the dialog's controls instead of leaving it.

const CANDIDATE_SELECTOR = [
  "a[href]",
  "button",
  "input:not([type=hidden])",
  "select",
  "textarea",
  "summary",
  "[tabindex]",
  "[contenteditable]:not([contenteditable=false])",
].join(",");

// Controls reachable with Tab, in document order: enabled, rendered, not
// inert and not removed from the tab order.
export function tabbableElements(root: HTMLElement): HTMLElement[] {
  return Array.from(root.querySelectorAll<HTMLElement>(CANDIDATE_SELECTOR)).filter(
    (element) =>
      element.tabIndex >= 0 &&
      !element.matches(":disabled") &&
      !element.closest("[inert]") &&
      element.getClientRects().length > 0,
  );
}

// Where Tab should move focus in a dialog with `count` tabbable elements.
// `activeIndex` is the focused element's index, or -1 when focus is on the
// dialog itself or outside it. Returns the index to focus, -1 to keep focus on
// the dialog when it has no controls, or null to let the browser move focus.
export function focusTrapTarget(count: number, activeIndex: number, backwards: boolean): number | null {
  if (count === 0) return -1;
  if (backwards) return activeIndex <= 0 ? count - 1 : null;
  return activeIndex === -1 || activeIndex === count - 1 ? 0 : null;
}

// Escape closes the dialog unless something inside already handled it, or an
// input method editor is using it to cancel a composition.
export function isDialogDismissal(event: Pick<KeyboardEvent, "key" | "defaultPrevented" | "isComposing">) {
  return event.key === "Escape" && !event.defaultPrevented && !event.isComposing;
}
