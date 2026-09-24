import assert from "node:assert/strict";
import test from "node:test";
import { focusTrapTarget, isDialogDismissal } from "../lib/focus-trap.ts";

test("Tab wraps from the last control to the first and Shift+Tab the other way", () => {
  assert.equal(focusTrapTarget(3, 2, false), 0);
  assert.equal(focusTrapTarget(3, 0, true), 2);
});

test("Tab between inner controls keeps the browser's order", () => {
  assert.equal(focusTrapTarget(3, 0, false), null);
  assert.equal(focusTrapTarget(3, 1, false), null);
  assert.equal(focusTrapTarget(3, 1, true), null);
  assert.equal(focusTrapTarget(3, 2, true), null);
});

test("focus on the dialog itself or outside it moves to the first or last control", () => {
  assert.equal(focusTrapTarget(3, -1, false), 0);
  assert.equal(focusTrapTarget(3, -1, true), 2);
  assert.equal(focusTrapTarget(1, 0, false), 0);
  assert.equal(focusTrapTarget(1, 0, true), 0);
});

test("a dialog without controls keeps focus on itself", () => {
  assert.equal(focusTrapTarget(0, -1, false), -1);
  assert.equal(focusTrapTarget(0, -1, true), -1);
});

test("only an unhandled Escape outside a composition dismisses the dialog", () => {
  const key = (key, extra = {}) => ({ key, defaultPrevented: false, isComposing: false, ...extra });
  assert.equal(isDialogDismissal(key("Escape")), true);
  assert.equal(isDialogDismissal(key("Escape", { defaultPrevented: true })), false);
  assert.equal(isDialogDismissal(key("Escape", { isComposing: true })), false);
  assert.equal(isDialogDismissal(key("Enter")), false);
  assert.equal(isDialogDismissal(key("Tab")), false);
});
