# Users and roles

Everyone who signs in to MeshRMM is a user, and what a user may do comes
from their roles. A role is a list of permissions and the companies it
covers. Users get roles directly (Settings → Users), through single sign-on
groups, or through SCIM groups.

## Built-in roles

- **Administrator** holds every permission in every company. It can't be
  deleted, emptied or limited to some companies, and the server always
  keeps an enabled administrator who can sign in (see
  [Architecture](architecture.md#sign-in-and-permissions)).
- **Technician** is an editable default for day-to-day device work.

Other roles are built from the permissions in Settings → Roles. Nobody can
grant more than they hold: a role editor can give a role only permissions
they hold themselves in every company the role covers (`403
permission_escalation`). Assigning roles to users follows the same rule.

## Company scope

Each role covers **every company** (the default) or **only some
companies**. A role limited to Acme lets its users work with Acme's devices
and nothing else: a managed service provider can give each team of
technicians its own customers.

Permissions are judged for each device. A user holds a permission on a
device when one of their roles grants that permission **and** covers the
device's company. Holding a permission through one role and a company
through another is not enough: a user with "view devices in Acme" and "view
and edit devices in Globex" can view both companies' devices but edit only
Globex's. A device in no company is reached only by roles over every
company.

What a limited role reaches, it reaches everywhere in MeshRMM; what it
doesn't, is hidden as if it didn't exist (`404`, with the same answer as a
device that was never enrolled):

- the device wall, and the live updates the website's event socket sends
  (a device moved out of reach disappears from it, one moved in appears);
- a device's pages: details, notes, tags, resource use, screen image,
  system information, monitors, updates, applications and history;
- the fleet views: software search, the Updates page, outdated applications
  and their counts;
- remote sessions, terminals, the file browser, Wake-on-LAN, script runs and
  file deliveries;
- jobs: a limited role runs jobs only on devices in reach. Whole companies
  keep only those in reach, tags resolve to the tagged devices in reach, and
  "every device" is refused (`403 permission_denied`) unless the role
  holding the needed permission and `jobs.run_wide` covers every company.
  A job none of whose devices are in reach is hidden, and a job's other
  devices are left out of its pages.
- alerts: the list, counts, an alert's page and its self-healing attempts,
  and the alerts the event socket sends. Alert rules themselves are
  instance-wide, so every holder of `alerts.view` sees the list of rules;
- enrollment: installers and deployment tokens only for companies in reach.
  A limited user must choose a company for an installer (`400
  company_required`), and sees and revokes only their companies' tokens;
- the companies list, which shows only companies in reach;
- tags: every tag is listed, with the number of devices in reach.
- custom field values on devices and companies, revealing secrets, setting
  many devices at once and the device wall's filter by field;
- remote tools: processes, services and the registry;
- maintenance windows: the list, a device's windows, and starting, changing
  and ending them, with "every device" refused as for jobs;
- reports: a limited user reports on chosen companies or tags in reach, never
  every device, and sees only the reports they made, while what those
  covered stays in reach; see [Reports](reports.md#permissions-and-audit).

Moving a device to another company, or deleting a company, takes effect at
once: open terminals and remote sessions on a device that leaves a user's
reach end, and the device leaves their device wall.

### Instance-wide permissions

Some permissions act on what the whole instance shares rather than on a
company's devices. Only roles over every company grant them; a role
limited to some companies can't be saved with one (`400
instance_permission_on_limited_role`), and the role editor disables them
while "Only some companies" is chosen. If one is on a limited role anyway
(written into the database by hand), it grants nothing.

| Permission | Why it is instance-wide |
| --- | --- |
| `companies.manage` | Creates and deletes companies and moves devices between any of them. |
| `tags.manage` | Tags are shared by every company. |
| `custom_fields.manage` | Field definitions are shared by every company. |
| `scripts.manage_shared`, `files.manage_shared` | Shared toolbox items run in every company. |
| `automation.manage` | Policies run on any device. |
| `applications.manage` | The application library and its custom installers are shared. |
| `alerts.manage` | Alert rules and notification channels cover every company. |
| `reports.manage` | Report schedules run on any company's devices and email anyone. |
| `users.manage`, `roles.manage` | Change who may do what everywhere. |
| `settings.manage`, `authentication.manage` | Instance settings and sign-in policy. |
| `api_tokens.manage` | Sees and revokes every user's tokens. |
| `audit.view` | The audit log records every company. |

Every other permission is judged per device (deployment tokens per
company); `GET /v1/permissions` lists each with its `reach`, `company` or
`instance`. The classification lives next to each permission in
`server/src/rbac.rs`.

Some actions lend a permission to something that later runs on any device,
so they need the lent permission in every company, not just one:
saving an automation policy, an alert rule that runs a script, and an
application with a custom installer (which runs as SYSTEM or root). Turning
such a rule off is always allowed.

Seeing other users' jobs, as the audit log would, needs `audit.view`, so
limited users see their own jobs only.

### API tokens and single sign-on

An API token acts as its user, so it reaches what its user reaches; a
token limited to some permissions keeps each permission's companies. SCIM
and single sign-on map groups to roles as before; the role decides the
companies.

## API

| Route | Notes |
| --- | --- |
| `GET /v1/roles` | Each role has `all_companies` and `company_ids` (empty when it covers every company). |
| `POST /v1/roles` | `{name, description, permissions, all_companies, company_ids}`. `all_companies` defaults to `true`; with `false`, `company_ids` must name at least one company (`400 invalid_role_companies`, `404` for one that doesn't exist). |
| `PATCH /v1/roles/{id}` | Any of the same fields. Naming `company_ids` alone limits the role to them; `all_companies: true` covers every company again. |
| `GET /v1/permissions` | Every permission with its `description` and `reach`. |
| `GET /v1/account` | `permissions` are those the user holds somewhere; `limited_permissions` are those they hold only in some companies, which the website uses to hide "every device" choices. |

Creating and changing a role is audited as `role.create` and `role.update`;
both record `all_companies` and `company_ids`, and an update that changes
them also records `previous_companies`.

On the event socket (`/v1/events`) a limited user's snapshot holds only the
devices and companies in reach. Changes to devices or companies out of reach
arrive as `{"type": "skipped", "revision": n}`, which only moves the
revision on, so the website can still tell when it missed an event.

## Code

`server/src/access.rs` holds the rules: `Access` maps each permission a
user holds to the companies it covers (`CompanySet`), built from their roles
by `Access::of`. `Authorized` (every request's caller) carries it, with
`require_device` and `require_company` for single items (`403` without the
permission anywhere, `404` out of reach) and `devices_condition` and
`companies_condition` for SQL filters, so lists are filtered in one query.
`server/tests/company_scope.rs` covers a limited technician across the
device-facing routes, and `server/tests/query_plans.rs` the indexes the
filtered queries use.
