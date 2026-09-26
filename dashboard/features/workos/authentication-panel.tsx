"use client";

import { AdminPortalDomainVerification, AdminPortalSsoConnection } from "@workos-inc/widgets";
import { useWorkspace } from "../workspace/workspace-context";
import { AdministratorsOnly } from "./users-panel";

export function AuthenticationPanel() {
  const { isAdmin, getAccessToken } = useWorkspace();
  if (!isAdmin) return <AdministratorsOnly />;
  return (
    <div className="management-stack">
      <section className="management-panel"><div className="management-heading"><h2>Company domains</h2><p>Verify a company domain before routing its users through SSO.</p></div><AdminPortalDomainVerification authToken={getAccessToken} /></section>
      <section className="management-panel"><div className="management-heading"><h2>Identity provider</h2><p>Configure and maintain this company&apos;s SAML or OIDC connection.</p></div><AdminPortalSsoConnection authToken={getAccessToken} /></section>
    </div>
  );
}
