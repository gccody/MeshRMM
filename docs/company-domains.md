# Company domains and provisioning

MeshRMM uses shared Cloudflare infrastructure with a hard tenant boundary at the
hostname and data layers:

- `meshrmm.com` is the public marketing site and has no company sign-in UI.
- `admin.meshrmm.com` is the platform-owner console.
- `auth.meshrmm.com/login` validates WorkOS invitation links and sends each
  invitee to the company encoded by the invitation's WorkOS organization.
- `<immutable-slug>.meshrmm.com` is a company dashboard and Agent control-plane
  endpoint.
- `api.meshrmm.com` remains a compatibility endpoint for already-enrolled
  Agents and native clients during migration.

The dashboard Worker resolves the hostname in D1 before rendering. The API then
independently compares the hostname's company with the `org_id` in a verified
WorkOS access token, or with the company on an Agent credential. A token for one
company therefore cannot be used on another company's hostname. Users who
belong to multiple companies must visit the other company's URL and
re-authenticate; the application intentionally contains no organization
switcher.

## One-time production setup

1. In Cloudflare DNS, create a proxied wildcard record for `*.meshrmm.com`. The
   dashboard's `*.meshrmm.com/*` Worker route then handles every company slug,
   so creating a company does not require a DNS API call or a new deployment.
2. In the WorkOS application, add `https://*.meshrmm.com` as a non-default
   Redirect URI. Keep `https://meshrmm.com` as the exact default redirect and
   sign-out URI. WorkOS production environments do not accept wildcard sign-out
   URIs, so every MeshRMM surface signs out to the marketing domain.
3. Set the WorkOS application Sign-in URL to
   `https://auth.meshrmm.com/login`. This makes invitation links enter the
   company-aware invitation broker.
4. Add `https://admin.meshrmm.com` as an allowed WorkOS CORS origin. Company
   origins are registered automatically while each company is provisioned.
5. Replace `user_REPLACE_WITH_PLATFORM_OWNER` in `server/wrangler.jsonc` with
   the immutable WorkOS user ID allowed to use the owner console. More than one
   break-glass owner may be supplied as a comma-separated list.
6. Store the WorkOS API key as a Worker secret; never place it in
   `wrangler.jsonc`:

   ```sh
   cd server
   npx wrangler secret put WORKOS_API_KEY
   ```

7. Apply D1 migrations before deploying either Worker, then deploy the server
   before the dashboard:

   ```sh
   cd server
   npx wrangler d1 migrations apply DB --remote
   npx wrangler deploy

   cd ../dashboard
   npm run deploy
   ```

## What company creation does

`admin.meshrmm.com` accepts a company name, permanent DNS slug, and initial
administrator email. The server persists a retryable provisioning operation,
then:

1. creates or recovers a WorkOS organization using the MeshRMM company UUID as
   its WorkOS external ID;
2. creates the MeshRMM `agents:manage` and `company:settings:manage`
   permissions if needed;
3. creates or repairs the environment-level `company_admin` role with both
   MeshRMM permissions and the WorkOS user, domain-verification, and SSO Widget
   permissions;
4. registers the exact company origin in WorkOS CORS;
5. invites the first administrator with the `company_admin` role; and
6. moves the company to `awaiting_admin`, then to `active` on its first valid
   company-scoped session.

Failures are retained in D1 and can be retried from the owner console. WorkOS
resources are looked up before creation, making retries safe after partial
completion. Suspending a company immediately stops dashboard and Agent
control-plane authorization. Billing is deliberately absent from company
settings and remains a platform-owner responsibility.

Companies created before the domain migration appear in the owner console with
an empty slug. The owner can assign each one a slug exactly once; the API rejects
all later changes.

## WorkOS authentication-policy boundary

Company administrators can invite and remove users, assign roles, verify
domains, and configure SAML/OIDC through the embedded WorkOS Widgets. WorkOS
also automatically disables non-SSO methods when a company first configures an
SSO connection.

WorkOS currently exposes organization authentication policies (individual
password, Magic Auth, social-login, and per-organization MFA choices) only in
the WorkOS Dashboard, not through its public API or the available Widgets. The
application does not display policy controls that it cannot enforce. Fully
delegating those remaining switches requires either a future WorkOS policy API
or replacing hosted AuthKit with a custom authentication UI and policy engine.
