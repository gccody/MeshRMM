"use client";

import { LoaderCircle, X } from "lucide-react";
import { useEffect, useState } from "react";
import type { ViewerLaunch } from "./use-remote-handoff";
import { useViewerPlatform } from "./use-viewer-platform";
import { ViewerDownloadLinks } from "./viewer-download-links";

// A notice that the viewer seems to have opened goes away by itself.
const SETTLED_NOTICE_MS = 10_000;

// Progress and recovery for the latest Connect. Detection is a guess (see
// viewer-launch.ts), so every outcome keeps "Try again" and the download, and
// the copy never states that the viewer is missing.
export function ViewerLaunchNotice({ launch, offline = false, onRetry, onDismiss }: {
  launch: ViewerLaunch;
  // Like Connect, Try again waits until the browser is back online.
  offline?: boolean;
  onRetry: () => void;
  onDismiss: () => void;
}) {
  const platform = useViewerPlatform();
  const [showDownloads, setShowDownloads] = useState(false);
  const settled = launch.phase === "handed-off" || launch.phase === "unknown";

  useEffect(() => {
    if (!settled || showDownloads) return;
    const timer = window.setTimeout(onDismiss, SETTLED_NOTICE_MS);
    return () => window.clearTimeout(timer);
  }, [onDismiss, settled, showDownloads]);

  if (launch.phase === "opening") {
    return (
      <div className="viewer-launch-notice" role="status">
        <LoaderCircle size={16} className="spin" aria-hidden="true" />
        <span>Opening MeshRMM Remote for {launch.agentName}…</span>
      </div>
    );
  }

  if (settled) {
    return (
      <div className="viewer-launch-notice" role="status">
        <div className="viewer-launch-body">
          <p>
            Continue in MeshRMM Remote. Didn’t open?{" "}
            <button type="button" className="link-button" onClick={onRetry} disabled={offline} title={offline ? "You’re offline" : undefined}>Try again</button>
            {" · "}
            <button type="button" className="link-button" onClick={() => setShowDownloads((shown) => !shown)} aria-expanded={showDownloads}>Get MeshRMM Remote</button>
          </p>
          {showDownloads && <ViewerDownloadLinks platform={platform} withSetup />}
        </div>
        <button type="button" className="viewer-launch-dismiss" onClick={onDismiss} aria-label="Dismiss"><X size={16} /></button>
      </div>
    );
  }

  return (
    <div className="viewer-launch-notice not-detected" role="status">
      <div className="viewer-launch-body">
        <p>
          <strong>MeshRMM Remote hasn’t opened yet.</strong>{" "}
          If your browser asks to open MeshRMM Remote, choose <strong>Open</strong>. If nothing appears, it may not be
          installed on this computer.
        </p>
        <ViewerDownloadLinks platform={platform} withSetup />
        <div className="viewer-launch-actions">
          <button type="button" className="secondary-button" onClick={onRetry} disabled={offline} title={offline ? "You’re offline" : undefined}>Try again</button>
          <button type="button" className="link-button" onClick={onDismiss}>Dismiss</button>
        </div>
      </div>
    </div>
  );
}
