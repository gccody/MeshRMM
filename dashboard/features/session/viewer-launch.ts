// Guesses whether the browser handed a meshrmm: link to the native viewer. A
// page cannot see whether a link handler exists; when the viewer (or the
// browser's "Open MeshRMM Remote?" prompt, in some browsers) takes over, the
// page loses focus or is hidden, and that is the only signal there is.
export type ViewerLaunchOutcome = "handed-off" | "not-detected" | "unknown";

// Without a signal by then, the viewer probably did not open.
export const LAUNCH_DETECT_MS = 2_500;
// The handoff token's lifetime (HANDOFF_TTL_MS in server/src/lib.rs). A viewer
// that opens later cannot redeem it, so later signals mean nothing.
export const LAUNCH_WATCH_MS = 60_000;

export type ViewerLaunchEnv = {
  window: EventTarget;
  document: EventTarget & { readonly visibilityState: string; hasFocus(): boolean };
  setTimeout: (callback: () => void, delayMs: number) => unknown;
  clearTimeout: (handle: unknown) => void;
};

export function browserLaunchEnv(): ViewerLaunchEnv {
  return {
    window,
    document,
    setTimeout: (callback, delayMs) => window.setTimeout(callback, delayMs),
    clearTimeout: (handle) => window.clearTimeout(handle as number),
  };
}

// Start before opening the link. Reports "unknown" at once when the page is
// already hidden or unfocused, "handed-off" when it loses focus or is hidden
// within LAUNCH_DETECT_MS, and otherwise "not-detected"; a later signal before
// LAUNCH_WATCH_MS upgrades that to "handed-off". Returns a cancel function
// after which nothing is reported.
export function watchViewerLaunch(env: ViewerLaunchEnv, onOutcome: (outcome: ViewerLaunchOutcome) => void): () => void {
  if (env.document.visibilityState === "hidden" || !env.document.hasFocus()) {
    onOutcome("unknown");
    return () => {};
  }

  let active = true;
  const handedOff = () => {
    if (!active) return;
    stop();
    onOutcome("handed-off");
  };
  const visibilityChanged = () => {
    if (env.document.visibilityState === "hidden") handedOff();
  };
  const detectTimer = env.setTimeout(() => {
    if (active) onOutcome("not-detected");
  }, LAUNCH_DETECT_MS);
  const watchTimer = env.setTimeout(() => stop(), LAUNCH_WATCH_MS);

  function stop() {
    active = false;
    env.clearTimeout(detectTimer);
    env.clearTimeout(watchTimer);
    env.window.removeEventListener("blur", handedOff);
    env.window.removeEventListener("pagehide", handedOff);
    env.document.removeEventListener("visibilitychange", visibilityChanged);
  }

  env.window.addEventListener("blur", handedOff);
  env.window.addEventListener("pagehide", handedOff);
  env.document.addEventListener("visibilitychange", visibilityChanged);
  return stop;
}

let openingExternalLink = false;

// Opens a link that another app handles. Browsers fire beforeunload for it
// although the page stays, so leave-page guards check isOpeningExternalLink()
// to avoid asking about unsaved changes that are not at risk.
export function openExternalLink(url: string, location: Pick<Location, "assign"> = window.location) {
  openingExternalLink = true;
  try {
    location.assign(url);
  } finally {
    openingExternalLink = false;
  }
}

export function isOpeningExternalLink() {
  return openingExternalLink;
}
