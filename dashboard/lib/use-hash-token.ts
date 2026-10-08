import { useSyncExternalStore } from "react";
import { tokenFromHash } from "../features/auth/next-path";

const subscribe = (onChange: () => void) => {
  window.addEventListener("hashchange", onChange);
  return () => window.removeEventListener("hashchange", onChange);
};

// The link's token: undefined while unknown (the prerendered page and the
// first render, which must match it), then the token or null.
export function useHashToken(): string | null | undefined {
  return useSyncExternalStore(subscribe, () => tokenFromHash(window.location.hash), () => undefined);
}
