# Modular product entitlements plan

Lets each company buy or enable product modules separately: remote access today, then RMM
(monitoring, scripting, patching), ticketing and whatever follows. The work happens on the
integration branch `modular-entitlements`, cut from `main` at `8315d21`. Line numbers were taken
at `8315d21` and will drift; search for the quoted code rather than trusting the number.

The tasks are meant to be implemented by separate agent threads. Each task section is written to
stand alone; read [Design](#design), [Working rules](#working-rules), [Merge order](#merge-order)
and [File overlap](#file-overlap) first, then only your own task's section.

## Status

| Task | Area | Wave | Depends on | Status | Commit(s) |
| --- | --- | --- | --- | --- | --- |
| M1 Schema, `Module` type, account response | Server | 1 | — | Not started | |
| M2 Enforce the remote module | Server | 2 | M1 | Not started | |
| M3 Platform module management (API + console) | Server, dashboard | 2 | M1 | Not started | |
| M4 Dashboard gating and locked states | Dashboard | 2 | M1 | Not started | |
| M5 Push enabled modules to the Agent | Protocol, server, Agent | 3 | M1, M3 | Deferred until a module needs Agent behavior | |
| M6 Device and technician limits | Server, dashboard | 3 | M1, M3 | Deferred until pricing is decided | |
| M7 Self-serve billing (Stripe) | Server, dashboard | 4 | M1, M3, M6 | Deferred; needs its own design | |

## Background

Today MeshRMM is one product. Nothing records what a company has purchased: a company is either
usable or not through `companies.status` (`provisioning`, `awaiting_admin`, `active`, `suspended`,
`failed`), and every feature is available to every active company. Relevant pieces:

- **Routing** is one `match` in `route` (`server/src/lib.rs`, around line 188). Remote routes
  already live mostly under `/v1/remote/...`; `POST /v1/agents/{id}/close-session` is the
  exception.
- **Company liveness checks** are repeated inline in SQL as
  `companies.status IN ('active', 'awaiting_admin')`: in `redeem_handoff`
  (`server/src/routes/handoffs.rs`), `redeem_agent_installer` (`server/src/routes/agents.rs`),
  `device_is_active` (`server/src/infrastructure.rs`), `CompanyPresence` and
  `AgentCoordinator::revoke_if_company_inactive`.
- **Remote session lifetime.** `create_handoff` → `redeem_handoff` → `create_session_for_device`
  starts a `RemoteSession` Durable Object. The object re-checks `device_is_active` on every idle
  refresh (`RemoteSession::refresh_idle_deadline`), which is how suspension stops live sessions.
- **User authorization** comes from WorkOS claims. `Identity::has_permission`
  (`server/src/lib.rs`) returns true for any `admin`/`company_admin` role regardless of the
  permission asked for.
- **Platform console** (`admin.meshrmm.com`, `server/src/routes/platform.rs`,
  `dashboard/features/platform/platform-dashboard.tsx`) creates, suspends and activates companies
  and writes `platform_audit_events`.
- **Dashboard pages** are a fixed list in `dashboard/features/workspace/views.ts`; the sidebar is
  in `dashboard/features/workspace/workspace-shell.tsx` (`<nav aria-label="Primary navigation">`).
- **Agent commands** (`AgentCommand` in `crates/protocol-types/src/signaling.rs`) are parsed in
  `agent/src/remote/mod.rs` (search `from_str::<AgentCommand>`). A message that doesn't parse
  falls through to `AgentSessionRequest`, fails, and is logged as
  `discarding invalid remote-session request` and skipped, so released Agents tolerate new
  command types.

## Design

### Concepts

- **Core** is always available to an active company and is not a module: companies, users and
  SSO, devices (enrollment, inventory, deletion, token rotation), company settings that aren't
  module-specific, audit.
- A **module** is a separately sold capability. The first is `remote` (remote sessions, background
  sessions, handoffs, remote session defaults in settings). Future: `rmm`, `ticketing`.
- An **entitlement** is a company's right to use a module: a row in `company_modules`.
- **Entitlements and permissions are separate checks.** Entitlement answers "did the company buy
  this"; WorkOS permissions answer "may this user do this". Never route an entitlement check
  through `Identity::has_permission`, because company admins pass every permission check there.
- `companies.status` stays the master switch. A suspended company has no modules, whatever
  `company_modules` says.

### Schema (`server/migrations/0016_company_modules.sql`)

```sql
CREATE TABLE company_modules (
  company_id TEXT NOT NULL,
  module TEXT NOT NULL CHECK (length(module) BETWEEN 1 AND 32),
  status TEXT NOT NULL CHECK (status IN ('active', 'trial', 'past_due', 'canceled')),
  expires_at INTEGER,              -- ms since epoch; NULL = no end date
  limits_json TEXT NOT NULL DEFAULT '{}',
  source TEXT NOT NULL DEFAULT 'manual' CHECK (source IN ('manual', 'stripe')),
  created_at INTEGER NOT NULL,
  updated_at INTEGER NOT NULL,
  PRIMARY KEY (company_id, module),
  FOREIGN KEY (company_id) REFERENCES companies(id) ON DELETE CASCADE
) STRICT, WITHOUT ROWID;

-- Every existing company keeps remote access.
INSERT OR IGNORE INTO company_modules (company_id, module, status, created_at, updated_at)
SELECT id, 'remote', 'active', created_at, COALESCE(updated_at, created_at) FROM companies;
```

- `module` deliberately has no `CHECK (module IN (...))`: adding a module must not need a SQLite
  table rebuild. The Rust `Module` enum is the allow-list; unknown values read from D1 are
  ignored.
- A module is **in effect** when
  `status IN ('active', 'trial', 'past_due') AND (expires_at IS NULL OR expires_at > now)`.
  `past_due` is the grace period while a payment is retried. `canceled` rows are kept for history
  rather than deleted.
- `limits_json` is unused until M6 (for example `{"devices": 50}` or `{"technicians": 5}`).
- `source` records who owns the row, so M7's billing sync doesn't overwrite manual grants.

### Server (`server/src/modules.rs`)

- `enum Module { Remote }`, serialized snake_case, with `as_str`/`parse`. Only modules that have
  code behind them get a variant.
- A pure `fn in_effect(status: &str, expires_at: Option<i64>, now: i64) -> bool`, unit-tested.
- `async fn company_modules(db, company_id, now) -> Result<Vec<CompanyModule>>` for the account
  response and platform list.
- `async fn require_module(db, company_id, module) -> Result<Option<Response>>`, returning the
  403 response described below when the module isn't in effect. Use it only where no existing
  query can absorb the check.
- **Prefer folding the check into an existing query** so it adds no D1 round trip.
  `server/tests/request_path.mjs` asserts round-trip counts (for example `handoff round trips` is
  2), so a separate lookup on the hot path will fail CI. Write the `EXISTS (...)` inline in each
  query literal: `server/tests/sql_regressions.py` compiles only string literals, so a `const`
  concatenated at runtime escapes the test.
- **Error contract** for a missing entitlement: HTTP 403 with
  `{"error": "Remote access is not enabled for this company.", "code": "module_not_enabled", "module": "remote"}`.
  `ApiError` in `server/src/infrastructure.rs` currently has only `error`; add the optional
  `code` and `module` fields (skipped when `None`) rather than a second error type.
- **Account response** (`GET /v1/account`) gains
  `modules: [{ "module": "remote", "status": "active", "expires_at_unix_ms": null }]`, listing only
  modules in effect, known to this build. Return an empty list when the company is not
  `active`/`awaiting_admin`.

### Conventions for future modules

- Routes under `/v1/<module>/...`, handlers in `server/src/routes/<module>.rs` (or a directory once
  it grows), gated once per route group.
- Tables prefixed with the module name (`ticketing_tickets`, `rmm_checks`); a module's migrations
  don't alter another module's tables. Core tables may be referenced by foreign key.
- Dashboard code under `dashboard/features/<module>/`; pages declare their module in `views.ts`.
- Links from one module into another (for example "connect" from a ticket) check the other
  module's entitlement in the UI and on the server, and render a locked state instead of failing.
- One Agent binary for everyone. Modules are switched on by the server (M5), not by separate
  builds, so signing, installers and self-update stay single-track.

## Working rules

- **One thread per task, one worktree per thread.** Create a worktree and sub-branch from the
  current tip of `modular-entitlements`, named `entitlements/<task-id>` (for example
  `entitlements/m1`). Do not work in another thread's worktree. When the task is done and
  verified, merge (no squash, no history rewrite) back into `modular-entitlements` in the
  [merge order](#merge-order), rebasing your sub-branch onto the integration branch first if it
  moved.
- Don't deploy, publish releases, bump versions, run remote D1 migrations, or push to `main`
  unless the user asks.
- Before changing code, read the code paths involved. If something in this plan turns out to be
  wrong, record why in your task's **Notes** and adjust, rather than forcing the plan.
- Match the surrounding style and comment density. Add tests for the behavior you change.
- Commit with conventional-commit subjects, then update the [Status](#status) table and your
  task's **Notes** in this file in the same merge.
- **Checks on the Mac:**
  - Rust: `cargo fmt --all -- --check`, `cargo clippy --locked --workspace --all-targets -- -D warnings`,
    `cargo test --locked --workspace`.
  - Server: also `cargo check --locked -p meshrmm-server --target wasm32-unknown-unknown`,
    `python3 server/tests/sql_regressions.py`, and every `node server/tests/*.mjs` that CI runs
    (`.github/workflows/ci.yml`; they need `worker-build --profile server-release` first). Use
    rustup's `cargo`; Homebrew's has no wasm32 target.
  - Dashboard: `cd dashboard && npm run verify`.
- **Windows.** M1–M4 and M6–M7 change only the server and dashboard, so they need no Windows
  endpoint validation. M5 changes the installed Agent and must follow
  [AGENTS.md](../AGENTS.md) (installed-service validation on DESKTOP-85R6S28).

## Merge order

1. **Wave 1:** M1. It defines the schema, `Module` type and API shape everything else uses.
2. **Wave 2:** M2, M3 and M4 in parallel once M1 is merged. Merge M2 first if two finish
   together; it has the most `server/src` overlap.
3. **Wave 3:** M5 and M6, when their deferral reasons are resolved.
4. **Wave 4:** M7.

## File overlap

| File | Tasks | Notes |
| --- | --- | --- |
| `server/src/lib.rs` | M1, M2, M3 | M1 adds `mod modules` and `AccountResponse.modules`; M3 adds platform routes. Keep edits local. |
| `server/src/infrastructure.rs` | M1, M2 | M1 extends `ApiError`; M2 changes `device_is_active` or adds a sibling. |
| `server/src/routes/platform.rs` | M3, M6, M7 | |
| `server/tests/sql_regressions.py` | M1, M2, M3 | Add new test methods; don't reorder existing ones. |
| `dashboard/features/workspace/types.ts` | M1 (contract only), M4 | M4 owns the TypeScript change. |
| `dashboard/features/platform/platform-dashboard.tsx` | M3, M6 | |
| `dashboard/features/agents/devices-panel.tsx` | M4, M6 | |

---

## M1 — Schema, `Module` type, account response

**Goal:** record entitlements and expose them, without changing any behavior yet.

1. Add `server/migrations/0016_company_modules.sql` as in [Schema](#schema-servermigrations0016_company_modulessql),
   including the `remote` backfill for every existing company.
2. Make newly created companies get `remote` too, so behavior is unchanged until M3 adds a
   choice: `create_platform_company` (`server/src/routes/platform.rs`) inserts the
   `company_modules` row in the same batch as the `companies` insert. Also cover any other place
   that inserts into `companies` (search `INSERT INTO companies`). Test fixtures in
   `server/tests/request_path.mjs`, `token_rotation.mjs` and `sql_regressions.py` insert companies
   directly; `presence.mjs` and `session_cleanup.mjs` build their own minimal schema. Update the
   fixtures whose queries M2 will gate so they keep remote access.
3. Add `server/src/modules.rs` with `Module`, `in_effect`, `company_modules` and `require_module`
   as in [Server](#server-serversrcmodulesrs). Extend `ApiError` with optional `code`/`module`.
4. Add `modules` to `AccountResponse` and `account_for_identity`
   (`server/src/routes/account.rs`). Fetch them in the same round trip as the company row if
   practical (a batch or a `json_group_array` subquery); if `request_path.mjs` counts account
   round trips, keep the count unchanged.
5. Tests: unit tests for `in_effect` and `Module` parsing (unknown values ignored);
   `sql_regressions.py` cases for the backfill, the in-effect predicate (expired trial, `past_due`,
   `canceled`), and cascade on company deletion.

**Done when:** all server checks pass and `GET /v1/account` returns `modules` containing `remote`
for existing and new companies.

**Notes:**

## M2 — Enforce the remote module

**Goal:** a company without `remote` in effect can't start, redeem or keep a remote session.

1. `create_handoff` (`server/src/routes/handoffs.rs`): add the entitlement `EXISTS` to the
   `INSERT ... SELECT ... WHERE EXISTS (...)`. Both "device not found" and "not entitled" then
   insert nothing; only on that failure path, run one query to tell them apart and return 403
   `module_not_enabled` or the existing 404. The success path must stay at 2 round trips.
2. `redeem_handoff`: add the entitlement `EXISTS` to both `UPDATE remote_handoffs ...` variants,
   next to the existing `companies.status` check. A handoff created before the module was
   disabled then fails to redeem.
3. Live sessions: `RemoteSession::refresh_idle_deadline` and `resume` rely on `device_is_active`.
   Add a remote-specific check (for example `remote_session_allowed(environment, device_id)` in
   `server/src/infrastructure.rs`, or an extra condition in the existing query used only by
   `RemoteSession`) so a disabled module ends sessions at the next activity check, the same way
   suspension does. Don't change `device_is_active` for callers unrelated to remote sessions.
4. Background sessions: confirm they start only through `create_handoff`/`redeem_handoff` with
   `start_in_background`; if any other path creates a `RemoteSession`, gate it too.
5. Leave `POST /v1/agents/{id}/close-session` ungated so a disabled company can still end
   sessions.
6. Tests: `sql_regressions.py` cases for each changed query (entitled, not entitled, expired
   trial, suspended company); extend `server/tests/request_path.mjs` with a company lacking
   `remote` (403 with `code`, no handoff row, round trips unchanged for the entitled path).

**Done when:** a company whose `remote` row is `canceled` or expired gets 403 `module_not_enabled`
from `POST /v1/remote/handoffs`, can't redeem an earlier handoff, and a live session ends at the
next idle refresh; entitled companies behave exactly as before.

**Notes:**

## M3 — Platform module management (API and console)

**Goal:** platform owners can grant, trial and revoke modules per company.

1. `GET /v1/platform/companies` includes each company's module rows (all statuses, with
   `expires_at` and `source`).
2. `PUT /v1/platform/companies/{company_id}/modules/{module}` with
   `{ "status": "active" | "trial" | "past_due" | "canceled", "expires_at_unix_ms": number | null }`.
   Authorize with `authorize_platform_owner`; reject unknown modules with 400; upsert with
   `source = 'manual'`; write a `platform_audit_events` row (`company.module_update`, with the
   module, old and new status and expiry in `metadata_json`). Return the updated company as
   `activate_platform_company` does.
3. `POST /v1/platform/companies` accepts an optional `modules` array (default `["remote"]`) and
   replaces M1's hard-coded `remote` insert.
4. When a module is canceled, live remote sessions end at their next idle refresh (M2). Ending
   them immediately (as `suspend_platform_company` revokes Agents) is optional; note the choice.
5. Console (`dashboard/features/platform/platform-dashboard.tsx`): per-company module chips with
   status and expiry, an editor for status/expiry, and module checkboxes on the create form.
   Follow the existing `mutateCompany` pattern and confirm before canceling.
6. Tests: server unit/SQL tests for the upsert and audit row; dashboard tests next to
   `dashboard/tests/platform-access.test.mjs`.

**Done when:** a platform owner can move a company's `remote` module between active, trial with
expiry, and canceled from the console, and the change is audited.

**Notes:**

## M4 — Dashboard gating and locked states

**Goal:** the company dashboard shows only what the company has, and explains what it lacks.

1. `dashboard/features/workspace/types.ts`: add
   `modules: { module: string; status: string; expires_at_unix_ms: number | null }[]` to
   `Account`, and a helper such as `hasModule(account, "remote")`. Treat a missing `modules`
   field (older server during deploy) as "remote enabled" so a dashboard released before the
   server doesn't lock everyone out.
2. `views.ts`: let a view declare an optional `module`. Core views (`agents`, `team`, `sso`,
   `settings`) have none today; the pattern is for future module pages.
3. Devices page (`dashboard/features/agents/`): when `remote` is not in effect, hide or disable
   Connect and background-session actions, and show a short locked notice ("Remote access isn't
   enabled for your company. Contact your MeshRMM administrator."). Keep inventory, enrollment
   and deletion working; those are core.
4. Settings (`dashboard/features/settings/`): the remote session defaults (blackout message,
   display border, session banner, connection notifications, idle lock) belong to `remote`.
   Show them read-only with the locked notice when the module is off.
5. Handle a 403 `module_not_enabled` from any action by showing the locked notice rather than a
   generic error (`dashboard/features/workspace/action-errors.ts`), since the module can be
   canceled while a page is open.
6. Trials: when `remote` is `trial` with an expiry, show a small "Trial ends <date>" note to
   company admins.
7. Tests: extend `dashboard/tests/workspace-views.test.mjs`, `account-load.test.mjs` and
   `action-errors.test.mjs`.

**Done when:** `npm run verify` passes, and with `remote` canceled the dashboard shows devices
without connect actions and a locked notice, while an entitled company sees no change.

**Notes:**

## M5 — Push enabled modules to the Agent (deferred)

**Deferred until** a module needs behavior on the endpoint (for example RMM monitoring
collectors). Remote access is enforced entirely by the server, so the Agent doesn't need to know
about it.

1. Add `AgentCommand::SetModules { modules: Vec<String> }` in
   `crates/protocol-types/src/signaling.rs`. Released Agents discard unknown commands (see
   [Background](#background)), but they log a warning each time; send it only to Agents new
   enough to understand it if that noise matters.
2. `AgentCoordinator` sends it after each Agent connects and whenever M3 changes the company's
   modules (fan out like `revoke_agents` in `server/src/routes/platform.rs`).
3. The Agent keeps the latest set in memory and starts or stops module-specific work from it.
   It must default to "nothing extra enabled" until the first `SetModules` arrives.
4. Windows installed-service validation per [AGENTS.md](../AGENTS.md).

**Notes:**

## M6 — Device and technician limits (deferred)

**Deferred until** pricing is decided: per device (typical for RMM), per technician (typical for
remote access and ticketing), or both.

1. Define the keys in `limits_json` (for example `devices`, `technicians`) and which module owns
   each.
2. Devices: enforce in `redeem_agent_installer` (`server/src/routes/agents.rs`) inside the claim
   query, counting undeleted agents, with a clear error for the installer. Decide whether
   existing devices over the limit keep working (recommended) or are blocked.
3. Technicians: enforce at invitation (WorkOS) and optionally report monthly active users from
   `company_active_users`.
4. Show usage against limits in the platform console and to company admins.

**Notes:**

## M7 — Self-serve billing (deferred)

**Deferred;** needs its own design doc before implementation. Outline:

- Stripe Customer per company, one Subscription with an item per module (and per-seat or
  per-device quantities from M6).
- A webhook endpoint (signature-verified) maps subscription state to `company_modules` rows with
  `source = 'stripe'`: `active`/`trialing` → `active`/`trial`, `past_due` → `past_due`,
  `canceled`/`unpaid` → `canceled`. Manual rows are never overwritten by the webhook.
- Customer portal or checkout links from the company dashboard for admins.
- Reconciliation from the scheduled handler in case a webhook is missed.

**Notes:**

## Open decisions

Defaults are in bold; change them here before the dependent task starts.

1. **Every existing and new company gets `remote`** until M3 makes it a choice. (M1)
2. Disabling a module ends live sessions **at the next idle refresh**, not immediately. (M2, M3)
3. Devices over a future device limit **keep working**; only new enrollments are blocked. (M6)
4. Per-module user permissions (for example `remote:connect`, `tickets:manage` as WorkOS
   permissions) are **out of scope** here; add them with the first module that needs finer
   roles.
5. Pricing units per module: **undecided**; blocks M6 and M7.
