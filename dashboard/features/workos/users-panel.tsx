"use client";

import dynamic from "next/dynamic";
import { useWorkspace } from "../workspace/workspace-context";
import { widgetLoading } from "./widget-loading";

// The widgets and their Radix Themes styles load only for administrators on
// this page.
const UsersWidgets = dynamic(() => import("./users-widgets"), { ssr: false, loading: widgetLoading("user management") });

export function UsersPanel() {
  const { isAdmin, getAccessToken } = useWorkspace();
  if (!isAdmin) return <AdministratorsOnly />;
  return <UsersWidgets authToken={getAccessToken} />;
}

export function AdministratorsOnly() {
  return <section className="management-panel"><p>Only company administrators can manage users and authentication.</p></section>;
}
