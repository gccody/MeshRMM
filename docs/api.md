# The MeshRMM API

The website is built on the same HTTP API that scripts and integrations
use: everything under `https://<your server>/v1`. This page covers what an
integrator needs: authenticating with an API token, errors, lists, and the
main flows with `curl`. The feature pages hold every route and field:
[devices](devices.md), [hardware and software](hardware-and-software.md#api),
[automation](automation.md#api), [patch management](patch-management.md#api),
[alerts](alerts.md#api), [monitors and self-healing](monitors.md#api),
[reports](reports.md#api),
[Wake-on-LAN](wake-on-lan.md#api) and
[the toolbox](toolbox.md).

The API is not versioned beyond `/v1` yet, and MeshRMM is still in heavy
development, so fields and routes may change between releases.

## API tokens

A personal API token acts as the user who made it. Make one on **Your
account → API tokens → New API token**: give it a name that says what will
use it, choose when it expires, and either let it do everything you can or
choose the permissions it needs. The website shows the token once; the
server keeps only its SHA-256 hash.

Tokens look like `mrmm_pat_` followed by 64 hex digits, so secret scanners
(GitHub's, gitleaks, trufflehog) can match `mrmm_pat_[0-9a-f]{64}` and catch
one committed by mistake.

Send it in the `Authorization` header:

```sh
export MESHRMM=https://rmm.example.com
export MESHRMM_TOKEN=mrmm_pat_…
curl -fsS -H "Authorization: Bearer $MESHRMM_TOKEN" "$MESHRMM/v1/api-tokens/current"
```

```json
{
  "token": {
    "id": "6f1c…",
    "name": "PSA ticket sync",
    "user": { "id": "0b9e…", "email": "ada@example.com", "display_name": "Ada" },
    "permissions": ["devices.view", "alerts.view", "alerts.respond"],
    "created_at": 1761100000000,
    "expires_at": 1763692000000,
    "last_used_at": 1761100300000,
    "last_used_ip": "203.0.113.7"
  },
  "permissions": ["devices.view", "alerts.view", "alerts.respond"]
}
```

`GET /v1/api-tokens/current` is the place to check a new integration's setup:
the top-level `permissions` are what the token may do right now.

### What a token may do

A token may do what its permissions allow **and** what its user's roles
grant at the moment of the request. Take a permission away from a role and
every token of its members loses it at once; disable or delete a user and
their tokens stop working. A token limited to some permissions is never
treated as the Administrator role, even if its user holds it, so signing
out an administrator needs an unlimited token of an administrator.

Some routes refuse tokens whatever their permissions (`403
api_token_not_allowed`), so that a leaked token can't create credentials
or access that outlast it or take over an account, and revoking it is
enough:

- signing in and out, and everything under `/v1/account` (profile,
  password, two-factor authentication, passkeys, sessions);
- making API tokens (`POST /v1/api-tokens`). Listing tokens and revoking
  any but the token itself need `api_tokens.manage`, its user's own tokens
  included, so a token limited to other permissions can't find or revoke
  them;
- the sign-in policy and API token settings (`/v1/settings/authentication`),
  single sign-on and SCIM settings;
- email settings (`/v1/settings/smtp`, reading and testing them included),
  since email carries every invitation and password reset link;
- inviting people, and renewing or revoking invitations
  (`/v1/invitations`); listing them works;
- password reset links, two-factor resets and unlinking single sign-on
  (`/v1/users/{id}/unlink-sso`) for other users;
- changing and deleting users (`PATCH` and `DELETE /v1/users/{id}`):
  renaming, disabling or enabling them, or replacing their roles. A deleted
  user may come back through single sign-on, enabled. Listing and reading
  users and signing them out (`POST /v1/users/{id}/sign-out`) work;
- creating, changing and deleting roles (`POST /v1/roles`, `PATCH` and
  `DELETE /v1/roles/{id}`); listing roles and permissions works;
- making and revoking [deployment tokens](devices.md#deployment-tokens)
  (`POST /v1/deployment-tokens`, `DELETE /v1/deployment-tokens/{id}`), which
  enroll devices until they expire; listing them works;
- the website's event socket (`/v1/events`), terminals, and starting remote
  sessions (`/v1/remote/handoffs`), which belong to a person at a browser.

The [file browser](files.md#how-it-works)'s routes, downloads and uploads
included, take tokens, so a script can fetch a device's logs or put a file
on it.

SCIM has its own tokens, made under **Authentication → Directory sync**; a
personal API token doesn't work at `/scim/v2`, and a SCIM token doesn't work
at `/v1`.

### Lifetimes and the server's policy

Under **Authentication → API tokens**, holders of `authentication.manage`
can turn API tokens off (every token is refused with `401
api_tokens_disabled` until they're turned on again; making new ones is
refused too) and set the longest lifetime a token may have, 365 days unless
changed. Lowering it shortens existing tokens that would last longer. Only
without a limit can users make tokens that never expire.

### Revoking

A user revokes their own tokens on their account page, and a token can
always revoke itself (`DELETE /v1/api-tokens/{id}`). Holders of `api_tokens.manage`
see every user's tokens under **Authentication → API tokens** and revoke any
of them. A revoked token stops working at once. Expired tokens stay listed
for 30 days so their users see why a script stopped, then disappear.

The list shows when each token was made and last used, from which address,
when it expires and what it may do. Last use is recorded at most once a
minute.

### Audit

Making and revoking tokens is recorded (`api_token.create`,
`api_token.revoke`). Everything done with a token is recorded as its user,
naming the token too: the audit page shows "API token “name”" under the
user, `GET /v1/audit` gives each event an `api_token` (`{id, name}` or
`null`), and `GET /v1/audit?api_token_id=<id>` lists what one token did.

### Routes

| Route | |
|---|---|
| `GET /v1/api-tokens` | The caller's tokens, the server's `policy` (`enabled`, `max_lifetime_days`) and the `permissions` a new token may be limited to. `?user_id=` for another user's and `?all=true` for everyone's (`api_tokens.manage`). With an API token, `api_tokens.manage` is needed for any of them. |
| `POST /v1/api-tokens` | Website only. `{name, expires_in_days, permissions}`: `expires_in_days` is required, `null` for never (only without a lifetime limit); `permissions` `null` or missing for all of the user's. `201` with the token and its `secret`, the only time it is shown. `400 lifetime_too_long`, `403 permission_escalation`, `403 api_tokens_disabled`, `409 too_many_tokens` (50 per user). |
| `DELETE /v1/api-tokens/{id}` | Revokes the caller's token, or anyone's with `api_tokens.manage`. An API token revokes itself, and any other token only with `api_tokens.manage`. |
| `GET /v1/api-tokens/current` | The token the request carries and what it may do now. `400 not_an_api_token` from the website. |

## Requests and errors

- Bodies are JSON (`Content-Type: application/json`); responses are JSON
  with `Cache-Control: no-store`.
- Times are Unix milliseconds. IDs are opaque strings.
- Requests with a token need no CSRF header. (The website's cookie requests
  must carry `X-MeshRMM-Request` and the server's `Origin` on every change;
  that still holds for them.)
- An error is the HTTP status and `{"error": "<message>", "code": "<code>"}`.
  The message is for people; act on the status and `code`, which is there
  when the status alone doesn't say enough.

| Status | Means |
|---|---|
| 400 | The request is invalid: a missing or wrong field, `invalid request: …` for JSON that doesn't fit, or a code such as `no_devices` |
| 401 | Not authenticated: `invalid_token` (unknown or revoked), `token_expired`, `user_disabled`, `api_tokens_disabled`, or `unauthenticated` with no credential at all |
| 403 | Authenticated but not allowed: `permission_denied` (the message names the permission), `api_token_not_allowed`, `permission_escalation` |
| 404 | No such thing, or one the caller can't see |
| 409 | The state forbids it: `device_offline`, `device_online`, `install_running` and the like |
| 429 | `rate_limited`; wait the seconds in `Retry-After` |

Unknown tokens count against the client's address like failed sign-ins: 30
in 15 minutes, after which unknown tokens from that address (an IPv6 /64)
get `429` until the oldest failure is 15 minutes old. Valid tokens from it
keep working, so one misconfigured script doesn't stop the other
integrations behind the same address. Behind a reverse
proxy, set `http.trusted_proxies` so the server sees clients' addresses
rather than the proxy's (`tls.trusted_proxies`, see
[self-hosting](self-hosting.md#behind-a-reverse-proxy-proxy)). A revoked
token counts, as the server no longer knows it; an expired one, a disabled
user's, or any token while tokens are turned off doesn't.

## Lists

Lists come in one of four shapes, each documented with its route:

- **Everything at once**, small enough not to page: devices
  (`/v1/agents`), tags, companies, rules, channels, policies, the toolbox.
- **The newest, up to `limit`**: jobs, script runs, update runs, alerts,
  application changes and wake requests take `limit` (each with its default
  and maximum, such as 50 and 200 for jobs) and return the newest first.
  Alerts add `truncated: true` when more matched; narrow the filters to see
  the rest.
- **Pages with an offset**: the fleet's updates (`/v1/updates/devices`),
  the device search (`/v1/system-info`) and the software searches
  (`/v1/software`, `/v1/software/devices`) take `limit` and `offset` and say
  how many match in `matching`; ask from the next offset until you have
  that many.
- **Pages with a cursor**: the audit log returns `{events, next}`; pass
  `next` as `before` for the next page, until `next` is missing.

Filters are query parameters and combine with AND.

## Examples

These use `$MESHRMM` and `$MESHRMM_TOKEN` from above, and
[`jq`](https://jqlang.org) to pick out fields.

### Devices

```sh
curl -fsS -H "Authorization: Bearer $MESHRMM_TOKEN" "$MESHRMM/v1/agents" |
  jq '.agents[] | {id, name, platform, connected, company_id, tag_ids, os: .system.os}'
```

`GET /v1/agents` (`devices.view`) returns every enrolled device with
whether its Agent is `connected`, its company, tags and a summary of its
hardware (`system`: operating system, model, serial number, IP addresses),
plus `companies` and `tags` to resolve their IDs. A device's ID
is the `id` other routes take as `{id}` and `device_ids`.

### Enrollment

| Route | |
|---|---|
| `POST /v1/agent-installers` | `devices.enroll`. `{platform, company_id}`: a single-use installer authorization (`{server, install_token, expires_at_unix_ms}`) that enrolls one computer within 30 minutes. |
| `GET /v1/deployment-tokens` | `devices.enroll`. Every [deployment token](devices.md#deployment-tokens), newest first: `id`, `name`, `company_id`, `company_name`, `created_by` (`{id, email}`, `email` `null` once the user is deleted), `created_at`, `expires_at`, `max_uses` (`null` for no limit), `uses`, `last_used_at`, `revoked_at` and `state` (`active`, `expired`, `revoked` or `exhausted`). Never the secret. Revoked and expired tokens stay listed until 30 days after they expire. |
| `POST /v1/deployment-tokens` | Website only, `devices.enroll`. `{name, company_id, expires_in_days, max_uses}`: `name` 1 to 80 characters; `company_id` required; `expires_in_days` 1 to 365, 30 when missing; `max_uses` 1 to 1,000,000, missing or `null` for no limit. `201` with `token` (as listed) and `bootstrap` (`{server, install_token, expires_at_unix_ms}`, what the installers embed), the only time the secret is shown. `404` for an unknown company. |
| `DELETE /v1/deployment-tokens/{id}` | Website only, `devices.enroll`. Revokes the token; `204` again for one already revoked. |
| `POST /v1/agent-installers/redeem` | For the Agent's installer, with the installer authorization or deployment token as its bearer token. Every refusal is `401 installer_rejected`, whichever it was and whyever, and counts against the same per-address limit as other one-time tokens (`429` past it). |

### System information

```sh
DEVICE=<device id>
curl -fsS -H "Authorization: Bearer $MESHRMM_TOKEN" "$MESHRMM/v1/agents/$DEVICE/system-info" |
  jq '{os: .os.name, version: .os.version, model: .hardware.model, serial: .hardware.serial_number}'
curl -fsS -H "Authorization: Bearer $MESHRMM_TOKEN" "$MESHRMM/v1/agents/$DEVICE/applications" | jq length
# Which devices have Firefox, in which versions:
curl -fsS -H "Authorization: Bearer $MESHRMM_TOKEN" "$MESHRMM/v1/software?q=firefox" | jq
```

`404 not_collected` means the device hasn't sent its first snapshot yet.
`POST /v1/agents/{id}/system-info/refresh` asks an online device for a new
one, which its Agent posts to `POST /v1/agents/{id}/system-info` with its own
credential; that route takes no API token. `GET /v1/system-info`, `/v1/software` and `/v1/software/devices` answer
a page at a time, with how many match.

### Scripts and jobs

A job runs a toolbox script (or sends a file, or scans for or installs
updates) on chosen devices, companies, tagged devices or every device.
Scripts come from the toolbox: the caller's own and shared ones.

```sh
# Find a script, or add one (scripts.run; shared ones need scripts.manage_shared).
curl -fsS -H "Authorization: Bearer $MESHRMM_TOKEN" "$MESHRMM/v1/toolbox" |
  jq '.scripts[] | {id, name, language}'
SCRIPT=$(curl -fsS -H "Authorization: Bearer $MESHRMM_TOKEN" -H "Content-Type: application/json" \
  -d '{"name": "Uptime", "language": "shell", "body": "uname -sr; uptime", "timeout_seconds": 60}' \
  "$MESHRMM/v1/toolbox/scripts" | jq -r .id)

# Run it on a Mac and a Linux server as root; skip any that are offline.
JOB=$(curl -fsS -H "Authorization: Bearer $MESHRMM_TOKEN" -H "Content-Type: application/json" \
  -d "{\"script_id\": \"$SCRIPT\", \"run_as\": \"system\", \"target\": \"devices\",
       \"device_ids\": [\"$MAC\", \"$LINUX\"], \"offline\": \"skip\"}" \
  "$MESHRMM/v1/jobs" | jq -r .id)

# Poll until finished_at_unix_ms is set, then read each device's output.
# Devices come 100 at a time; for more, add &after=<the last device_id>.
curl -fsS -H "Authorization: Bearer $MESHRMM_TOKEN" "$MESHRMM/v1/jobs/$JOB" |
  jq '{finished_at_unix_ms, counts}'
curl -fsS -H "Authorization: Bearer $MESHRMM_TOKEN" "$MESHRMM/v1/jobs/$JOB/devices" |
  jq '{matching, devices: [.devices[] | {device_name, outcome, detail, exit_code, run_id}]}'
curl -fsS -H "Authorization: Bearer $MESHRMM_TOKEN" "$MESHRMM/v1/script-runs/<run id>" |
  jq '{status, exit_code, stdout, stderr}'
```

```json
{
  "finished_at_unix_ms": 1761100058964,
  "counts": { "total": 2, "succeeded": 2, "failed": 0, "skipped": 0, "…": 0 }
}
{
  "matching": 2,
  "devices": [
    { "device_name": "Ada’s MacBook Air", "outcome": "succeeded", "detail": null, "exit_code": 0, "run_id": "50b9…" },
    { "device_name": "ubuntu-server", "outcome": "succeeded", "detail": null, "exit_code": 0, "run_id": "7759…" }
  ]
}
```

A script's `language` is `powershell` or `cmd` for Windows and `shell` (zsh
on macOS, bash on Linux) for the others. A job skips devices without its
interpreter (`"outcome": "skipped"`, `"detail": "Windows has no Shell
(zsh/bash) interpreter"`), so run one job per language for a mixed fleet. Targeting whole companies, tags or every device
needs `jobs.run_wide` as well. The [automation](automation.md) page covers
outcomes, offline devices, cancelling and scheduled policies.

### Alerts

```sh
# Open and acknowledged alerts, newest first.
curl -fsS -H "Authorization: Bearer $MESHRMM_TOKEN" "$MESHRMM/v1/alerts?state=active" |
  jq '.alerts[] | {id, severity, rule_name, device_name, detail, state}'
# Acknowledge one, then resolve it (alerts.respond).
curl -fsS -X POST -H "Authorization: Bearer $MESHRMM_TOKEN" "$MESHRMM/v1/alerts/<alert id>/acknowledge" | jq .state
curl -fsS -X POST -H "Authorization: Bearer $MESHRMM_TOKEN" "$MESHRMM/v1/alerts/<alert id>/resolve" | jq .state
# What a device's monitors last found, and an alert's self-healing attempts.
curl -fsS -H "Authorization: Bearer $MESHRMM_TOKEN" "$MESHRMM/v1/agents/<device id>/monitors" |
  jq '.[] | {rule_name, passing: .state.passing, detail: .state.detail}'
curl -fsS -H "Authorization: Bearer $MESHRMM_TOKEN" "$MESHRMM/v1/alerts/<alert id>/remediations" |
  jq '.[] | {attempt, action, status, exit_code, detail}'
```

### Tags

```sh
TAG=$(curl -fsS -H "Authorization: Bearer $MESHRMM_TOKEN" -H "Content-Type: application/json" \
  -d '{"name": "Servers", "color": "blue"}' "$MESHRMM/v1/tags" | jq -r .id)    # tags.manage
curl -fsS -H "Authorization: Bearer $MESHRMM_TOKEN" -H "Content-Type: application/json" \
  -d "{\"device_ids\": [\"$DEVICE\"], \"add\": [\"$TAG\"], \"remove\": []}" \
  "$MESHRMM/v1/device-tags"                                                   # devices.edit
```

Jobs, policies, alert rules and the software and update lists take
`tag_ids` (with `tag_match` `any` or `all`) or `tag=` to work on tagged
devices.

### Updates

```sh
# Pending updates on one device, and a fresh scan.
curl -fsS -H "Authorization: Bearer $MESHRMM_TOKEN" "$MESHRMM/v1/agents/$DEVICE/updates" |
  jq '{pending: .status.pending, security: .status.security, reboot_pending: .status.reboot_pending,
       updates: [.updates[] | {id, title, category, severity}]}'
curl -fsS -X POST -H "Authorization: Bearer $MESHRMM_TOKEN" "$MESHRMM/v1/agents/$DEVICE/updates/scan"
# Install the security updates, restarting only if needed.
curl -fsS -H "Authorization: Bearer $MESHRMM_TOKEN" -H "Content-Type: application/json" \
  -d '{"categories": ["security"], "reboot": "if_required", "reboot_delay_minutes": 15}' \
  "$MESHRMM/v1/agents/$DEVICE/updates/install"
```

`GET /v1/updates/devices` pages the devices' summaries, most urgent first,
with filters and the fleet's totals; a job with
`"updates": {"action": "install", …}` installs on many.

### Agent rollout

```sh
# Where the Agent release stands, then offer it to a quarter of the devices.
curl -fsS -H "Authorization: Bearer $MESHRMM_TOKEN" "$MESHRMM/v1/settings/agent-rollout" |
  jq '{release, mode, percent, paused, devices}'
curl -fsS -X PATCH -H "Authorization: Bearer $MESHRMM_TOKEN" -H "Content-Type: application/json" \
  -d '{"percent": 25}' "$MESHRMM/v1/settings/agent-rollout"
```

| Route | |
|---|---|
| `GET /v1/settings/agent-rollout` | `settings.manage`. `{release, mode, first_percent, percent, paused, offered_percent, devices, updated_at}`: `release` is the Agent release the server ships (`null` without downloads), `mode` `everyone` or `staged`, `offered_percent` the share of devices without the release that are offered it now (100 for everyone, 0 while paused), and `devices` `{on_release, offered, waiting}`: how many enrolled devices reported the release or newer at their last check, are offered it and don't have it, and aren't offered it yet (`null` without a release). |
| `PATCH /v1/settings/agent-rollout` | `settings.manage`. Any of `{mode, first_percent, percent, paused}`. Percentages are 1 to 100; `percent` and `paused` apply in staged mode only (`400` otherwise). Switching to `staged` starts the current release at `first_percent`, not paused, unless the request says otherwise. Answers like `GET`; recorded as `settings.agent_rollout_update`. |
| `POST /v1/agents/{id}/update-check` | The Agent, with its own credential: `{version}`, its version. `200` with the update manifest: naming the release when the device is offered it or has it, `{"schema_version": 3, "releases": {}}` otherwise and for a deleted device. `401 agent_unauthenticated` for any other credential, `404` without downloads. |

### Applications

```sh
# A device's outdated third-party applications.
curl -fsS -H "Authorization: Bearer $MESHRMM_TOKEN" "$MESHRMM/v1/agents/$DEVICE/packages" |
  jq '[.packages[] | select(.available_version) | {source, package_id, version, available_version}]'
# Update two of them (applications.deploy).
curl -fsS -H "Authorization: Bearer $MESHRMM_TOKEN" -H "Content-Type: application/json" \
  -d '{"action": "update", "mode": "only", "packages": ["winget:Mozilla.Firefox", "winget:Git.Git"]}' \
  "$MESHRMM/v1/agents/$DEVICE/application-runs"
```

`GET /v1/packages/outdated` lists a page of the fleet's outdated
applications with how many devices have each outdated, and
`GET /v1/packages/outdated/devices?package=` a page of one's devices; a job
with `"applications": {…}` installs, updates or uninstalls on many, and an
update can run where packages are outdated:

```sh
# The five applications outdated on the most devices.
curl -fsS -H "Authorization: Bearer $MESHRMM_TOKEN" "$MESHRMM/v1/packages/outdated?limit=5" |
  jq '{matching, packages: [.packages[] | {source, package_id, available_version, devices}]}'
# Update Firefox wherever it is outdated (applications.deploy).
curl -fsS -H "Authorization: Bearer $MESHRMM_TOKEN" -H "Content-Type: application/json" \
  -d '{"applications": {"action": "update", "mode": "only", "packages": ["winget:Mozilla.Firefox"]},
       "target": "outdated", "packages": ["winget:Mozilla.Firefox"], "offline": "skip"}' \
  "$MESHRMM/v1/jobs" | jq '{id, target_label, counts}'
```

See [application deployment](application-deployment.md#api).

### Wake-on-LAN

```sh
curl -fsS -H "Authorization: Bearer $MESHRMM_TOKEN" -H "Content-Type: application/json" \
  -d "{\"device_ids\": [\"$DEVICE\"]}" "$MESHRMM/v1/wakes" |
  jq '{wakes: [.wakes[] | {device_id, status, detail}], skipped}'
```

Online Agents on the sleeping device's network send the magic packet; see
[Wake-on-LAN](wake-on-lan.md) for what `status` means and what the device
needs.

## Webhooks and the API together

The API answers questions; [notification channels](alerts.md#notification-channels)
tell you when to ask. A webhook channel `POST`s JSON to your endpoint when
an alert is raised or resolved (`alert.raised`, `alert.resolved`), or a policy's job fails (`job.failed`), signed with
`X-MeshRMM-Signature` if you give it a secret. A typical integration:

1. Make a webhook channel pointing at the integration, with a secret, and
   choose it on the alert rules (and policies) it should hear about.
2. On each delivery, check the signature and the timestamp, then use the
   IDs in the body (`alert.id` and `alert.device_id`, or a failed job's `job.id`) to read
   more with the API: the device's system information, its pending updates,
   the job's outputs.
3. Act through the API: acknowledge the alert when a ticket is opened,
   resolve it when the ticket closes, run a fixing script as a job, or tag
   the device.

Use a token limited to what the integration does (for a ticketing system,
perhaps `devices.view`, `alerts.view` and `alerts.respond`), so a leak gives
away little, and give it its own name so the audit log says which
integration did what.
