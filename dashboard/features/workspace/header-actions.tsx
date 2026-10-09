import type { ReactNode } from "react";
import { createPortal } from "react-dom";
import { useWorkspace } from "./workspace-context";

// A page's own controls, shown beside its title in the shell's heading. The
// slot exists once the shell has rendered in the browser, so prerendered
// pages show the title alone.
export function HeaderActions({ children }: { children: ReactNode }) {
  const { headerSlot } = useWorkspace();
  return headerSlot ? createPortal(children, headerSlot) : null;
}
