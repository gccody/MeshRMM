
import { useSyncExternalStore } from "react";
import { type ViewerPlatform, detectViewerPlatform } from "./viewer-downloads";

// The platform never changes while the page is open.
const subscribe = () => () => {};

// The viewer build for this computer. The server and the first client render
// both use null, so hydration matches; the detected platform follows at once.
export function useViewerPlatform(): ViewerPlatform | null {
  return useSyncExternalStore(subscribe, () => detectViewerPlatform(navigator), () => null);
}
