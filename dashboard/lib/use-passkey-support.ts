import { useSyncExternalStore } from "react";
import { passkeysSupported } from "./webauthn";

// Support never changes while the page is open.
const subscribe = () => () => {};

// Whether this browser can use passkeys. The prerendered page and the first
// client render both say no, so hydration matches.
export function usePasskeySupport(): boolean {
  return useSyncExternalStore(subscribe, passkeysSupported, () => false);
}
