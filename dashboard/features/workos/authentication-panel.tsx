"use client";

import dynamic from "next/dynamic";
import { useWorkspace } from "../workspace/workspace-context";
import { AdministratorsOnly } from "./users-panel";
import { widgetLoading } from "./widget-loading";

// The widgets and their Radix Themes styles load only for administrators on
// this page.
const AuthenticationWidgets = dynamic(() => import("./authentication-widgets"), { ssr: false, loading: widgetLoading("authentication") });

export function AuthenticationPanel() {
  const { isAdmin, getAccessToken } = useWorkspace();
  if (!isAdmin) return <AdministratorsOnly />;
  return <AuthenticationWidgets authToken={getAccessToken} />;
}
