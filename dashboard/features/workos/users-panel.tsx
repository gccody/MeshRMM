"use client";

import { UsersManagement } from "@workos-inc/widgets";
import { useWorkspace } from "../workspace/workspace-context";

export function UsersPanel() {
  const { isAdmin, getAccessToken } = useWorkspace();
  if (!isAdmin) return <AdministratorsOnly />;
  return <section className="management-panel"><UsersManagement authToken={getAccessToken} /></section>;
}

export function AdministratorsOnly() {
  return <section className="management-panel"><p>Only company administrators can manage users and authentication.</p></section>;
}
