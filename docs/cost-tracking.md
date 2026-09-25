# Provider cost tracking

The owner console at `https://admin.meshrmm.com` shows what each company cost
on Cloudflare and WorkOS in the current or either of the previous two UTC
months. The report comes from `GET /v1/platform/costs?month=YYYY-MM`.

Every quantity is priced at the paid-plan list rate, even while the account is
on a free plan, and before any included allowance. For example, the first 10
million Workers requests a month are free on Workers Paid, but each one is
still charged at $0.30 per million. The report shows what a company would add
to a bill once the allowances are used up. It is not a copy of an invoice. The
rates and the date they were last checked are in `server/src/routes/costs.rs`
(`Item::rate` and `PRICING_AS_OF`).

## Setup

1. Create a Cloudflare API token with **Account > Account Analytics > Read**
   for the MeshRMM account. It reads the GraphQL Analytics API and the
   Workers Analytics Engine SQL API, and nothing else.
2. Store it as a server secret:

   ```sh
   cd server
   npx wrangler secret put CLOUDFLARE_ANALYTICS_API_TOKEN
   ```

3. Deploy the server with `node scripts/deploy-server.mjs`, which applies
   migration `0013_usage_metering.sql`, then publish the dashboard with the
   native release workflow. Both Workers write to the `meshrmm_usage`
   Analytics Engine dataset, which Cloudflare creates on the first write.

`CLOUDFLARE_ACCOUNT_ID` is set in `server/wrangler.jsonc`. Without the token,
the report still shows WorkOS costs, D1 storage and active users, and a warning
explains that Cloudflare usage is unavailable. Workers requests and D1 rows are
counted only from when metering is deployed, so the first month is partial.

## Where each figure comes from

| Line item | Source | Basis |
| --- | --- | --- |
| Workers requests | One Analytics Engine data point per Worker invocation. The dashboard Worker counts each request once. API requests that arrive through its service binding are not billed again, so only requests to `api.meshrmm.com` count at the API Worker. | Metered |
| Workers CPU time | Each Worker's CPU time from the GraphQL Analytics API, divided by each company's share of that Worker's invocations. Invocations from before metering began stay with the platform. | Allocated |
| Workers Logs events | One event per Worker and Durable Object invocation. Extra `console` lines are not counted. | Estimated |
| D1 rows read and written | The row counts D1 returns with every statement, added up per invocation (`MeteredStatement` in `server/src/usage.rs`). Durable Object events record their D1 rows under the object's ID, which the report maps to its company. | Metered |
| D1 storage | The database size, divided by each company's share of rows in the tables that grow with use. Prorated for a month in progress. | Allocated |
| Durable Objects requests, duration and SQLite rows | Per object from the GraphQL Analytics API. Company presence objects are named after their company. Agent coordinators and remote sessions are recorded in `usage_object_owners`. Incoming WebSocket messages bill at 20:1. | Metered |
| Durable Objects storage | Each namespace's stored bytes, divided by the number of that company's objects seen in the namespace. | Allocated |
| TURN relay egress | TURN credentials are tagged with the company ID (`customIdentifier`). The GraphQL Analytics API reports egress per identifier. | Metered |
| Analytics Engine data points | The metering's own writes. | Metered |
| Workers Paid plan | The $5 monthly base fee. | Fixed, platform |
| AuthKit monthly active users | Users who used a company's dashboard in the month (`company_active_users`), which is how WorkOS counts them. | Metered |
| SSO and Directory Sync connections | Active connections per WorkOS organization. Each is priced at the average of WorkOS's graduated volume tiers for the account's total. | Metered |

Usage that no company caused is shown as **platform overhead**. That covers
the marketing site, the owner console, sign-in, unauthenticated requests,
traffic before metering began, and objects or TURN traffic without an owner.

## Limits

- Analytics Engine keeps three months of data, so older months cannot be
  reported. Cloudflare may sample high volumes; the report scales samples back
  up with `_sample_interval`.
- A GraphQL query returns at most 10,000 groups. A month with more distinct
  Durable Objects than that would undercount.
- WorkOS bills a user once per environment. A user of two companies counts as
  an active user of each.
- Costs WorkOS bills per environment, such as a custom AuthKit domain, are not
  included.
