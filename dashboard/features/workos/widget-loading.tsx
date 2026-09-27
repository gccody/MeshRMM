"use client";

import { LoaderCircle, RefreshCw } from "lucide-react";

// Shown while a WorkOS widget bundle downloads, and if the download fails.
// Browsers remember a failed module import, so recovering takes a reload.
export function widgetLoading(label: string) {
  function WidgetLoading({ error }: { error?: Error | null }) {
    if (error) {
      return (
        <section className="management-panel widget-loading">
          <p role="alert">The {label} tools could not be loaded.</p>
          <button className="secondary-button" onClick={() => window.location.reload()}><RefreshCw size={16} /> Reload page</button>
        </section>
      );
    }
    return <section className="management-panel widget-loading"><p role="status"><LoaderCircle size={16} className="spin" /> Loading {label}…</p></section>;
  }
  return WidgetLoading;
}
