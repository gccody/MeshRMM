"use client";

import { WorkOsWidgets } from "@workos-inc/widgets/workos-widgets";
import "./workos-widgets.css";

// Radix Themes and the WorkOS widget context, around the widgets only. The
// page behind them already provides the background.
export function WidgetsScope({ children }: Readonly<{ children: React.ReactNode }>) {
  return (
    <WorkOsWidgets
      className="workos-widgets-scope"
      theme={{ accentColor: "violet", radius: "medium", fontFamily: "var(--font-geist-sans)", hasBackground: false }}
    >
      {children}
    </WorkOsWidgets>
  );
}
