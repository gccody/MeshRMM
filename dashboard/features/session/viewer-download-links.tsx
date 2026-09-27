"use client";

import { Download, MonitorDown } from "lucide-react";
import { useViewerPlatform } from "./use-viewer-platform";
import { VIEWER_DOWNLOADS, VIEWER_PLATFORMS, type ViewerPlatform } from "./viewer-downloads";

// The permanent sidebar entry point for the viewer, for every role.
export function ViewerDownloadCard() {
  const platform = useViewerPlatform();
  return (
    <section className="support-card viewer-card" aria-labelledby="viewer-card-title">
      <div className="support-icon"><MonitorDown size={16} aria-hidden="true" /></div>
      <strong id="viewer-card-title">MeshRMM Remote</strong>
      <p>Needed to connect to devices.</p>
      <ViewerDownloadLinks platform={platform} />
    </section>
  );
}

// This computer's build first; every build behind "Other platforms". With no
// detected platform, every build is listed.
export function ViewerDownloadLinks({ platform, withSetup = false }: { platform: ViewerPlatform | null; withSetup?: boolean }) {
  const all = (
    <ul className="viewer-download-list">
      {VIEWER_PLATFORMS.map((candidate) => (
        <li key={candidate}>
          <a href={VIEWER_DOWNLOADS[candidate].href} download>{VIEWER_DOWNLOADS[candidate].label}</a>
          {withSetup && <span>{VIEWER_DOWNLOADS[candidate].setup}</span>}
        </li>
      ))}
    </ul>
  );
  if (!platform) return <div className="viewer-downloads">{all}</div>;
  const download = VIEWER_DOWNLOADS[platform];
  return (
    <div className="viewer-downloads">
      <a className="viewer-download-primary" href={download.href} download><Download size={15} aria-hidden="true" />Download for {download.os}</a>
      {withSetup && <span className="viewer-download-setup">{download.setup}</span>}
      <details className="viewer-download-other">
        <summary>Other platforms</summary>
        {all}
      </details>
    </div>
  );
}
