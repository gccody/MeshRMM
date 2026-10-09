
import { Download } from "lucide-react";
import { VIEWER_DOWNLOADS, VIEWER_PLATFORMS, type ViewerPlatform } from "./viewer-downloads";

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
