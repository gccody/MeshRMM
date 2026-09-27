"use client";

import { AdminPortalDomainVerification } from "@workos-inc/widgets/admin-portal-domain-verification";
import { AdminPortalSsoConnection } from "@workos-inc/widgets/admin-portal-sso-connection";
import { WidgetsScope } from "./widgets-scope";

export default function AuthenticationWidgets({ authToken }: { authToken: () => Promise<string> }) {
  return (
    <WidgetsScope>
      <div className="management-stack">
        <section className="management-panel"><div className="management-heading"><h2>Company domains</h2><p>Verify a company domain before routing its users through SSO.</p></div><AdminPortalDomainVerification authToken={authToken} /></section>
        <section className="management-panel"><div className="management-heading"><h2>Identity provider</h2><p>Configure and maintain this company&apos;s SAML or OIDC connection.</p></div><AdminPortalSsoConnection authToken={authToken} /></section>
      </div>
    </WidgetsScope>
  );
}
