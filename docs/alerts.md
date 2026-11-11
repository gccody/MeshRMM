# Alerts

Alert rules watch the devices and raise alerts; notification channels tell
people about them by email or webhook. Most rules are judged on the server,
from what Agents already report: their connection, their resource usage
(every 5 seconds while someone watches the device, once a minute otherwise;
see [resource monitoring](devices.md#resource-monitoring)), the toolbox runs
they finish, and their
[update scans and installs](patch-management.md). [Monitor](monitors.md)
rules are checked by the device's Agent itself, and any rule whose alerts
resolve themselves, but device offline, can try to fix them automatically
([self-healing](monitors.md#self-healing)).

## In the website

The **Alerts** area has three pages:

- **Alerts** (`/alerts`): open alerts first, then acknowledged, then
  resolved, newest first within each. Filter by state (Active is open and
  acknowledged), severity, company and device; the filters are in the URL.
  **Acknowledge** tells others someone is on it; **Resolve** closes the
  alert. The navigation shows how many alerts are open, colored by the most
  severe.
- **Rules** (`/alert-rules`): every rule, with its condition, severity,
  which devices it covers and which channels it notifies.
- **Notification channels** (`/notification-channels`): email and webhook
  channels, how each one's last delivery went, and **Send test**.

A device's page lists its unresolved alerts and its latest resolved ones.
Every page updates live over the website's event socket.

## Rules

A rule has a name, a severity (info, warning or critical), a condition, and
covers every device, the devices of chosen companies, or the devices with
any (or all) of chosen [tags](devices.md#tags). Its devices are worked out
each time it is judged, so a device tagged after the rule was saved is
covered at once. It can be turned off.
Its condition can't change after it's created.

| Condition | Raises an alert when | Resolves itself when |
|---|---|---|
| Device offline | the device's Agent has been disconnected for at least N minutes (1 to 10,080) | the Agent connects |
| Low disk space | a volume's free space is below N percent, or below N GB (GiB, as the website shows sizes); one alert per volume | the volume has enough space again, or stops being reported |
| High CPU | the CPU load, averaged over the last M minutes (1 to 60), is above N percent | the average over the last M minutes is at or below N |
| High memory | the same for memory in use | the same |
| Script failed | a toolbox run on the device, by hand or as part of a [job](automation.md), couldn't start, timed out, or never reported back; optionally also a run that exited with a code other than 0 | never: a person resolves it |
| Updates pending | updates of a category, or of any, have been pending for at least N days (1 to 365), counted from the scan that first listed each | none of them is pending that long |
| Update install failed | an [update install](patch-management.md), by hand or as part of a job, failed partly or entirely, or never reported back | never: a person resolves it |
| Restart pending | the device's update scans have said for at least N days (1 to 365) that it must restart to finish installing updates | a scan says it no longer has to |
| Application install failed | an [application](application-deployment.md) install or update, by hand, in a job or by a policy, failed for an application, or never reported back; one alert per failed application | never: a person resolves it |
| Application updates pending | applications the device's package managers list as outdated, other than those the library pins, have been outdated for more than N days (1 to 365), counted from the scan that first found each | none of them is outdated that long |
| Monitors: service, process, event log, ping, TCP port, website, certificate, script check | the device's Agent finds the check failing N times in a row; see [Monitors](monitors.md) | a check passes |

The loads are averages so that a brief spike or dip neither raises nor
clears an alert. A load rule judges the M minutes up to the device's latest
reading, each reading weighted by the time it covers (the time since the
previous one), so the average is the same whether or not someone had the
device's page open during them. It needs readings covering its whole window,
from a connection that hasn't dropped, and a latest reading at most two
minutes old: until then, while the device is offline, and once it stops
reporting, its alerts stay as they are. A device nobody watches reports once
a minute, so its alert can come up to about a minute after the load
crossed the threshold. The server keeps each device's
readings only as far back as the longest window of the enabled CPU and
memory rules, and none while there is no such rule, so a rule created,
turned on or lengthened to look further back than that waits for its window
to fill, as for a device that just connected. A disk space rule judges the latest
reading, while the device is online. The update rules judge the device's
latest scan, online or not, and say nothing about a device that never
scanned.

A rule raises at most one unresolved alert per device (and per volume, for
disk space, and per application, for application installs). Another failure of a script on a device with an unresolved
script alert counts on that alert (`occurrences`) and makes it describe the
latest failure, without notifying again. A run the Agent never reports on is
marked `lost` two minutes after its timeout, and counts as a failure then. A
run refused at once because the device is offline isn't a failure: the user
who started it already saw the error.

Turning a rule off, changing its companies or tags, or moving or untagging a
device so that the rule no longer covers it, resolves its alerts
(resolution `not_applicable`). Deleting a rule
deletes its alerts. Deleting a device deletes its alerts.

## Alerts

An alert keeps the rule and device, the volume (`subject`) for disk space,
when it was raised, the value that raised it (minutes offline, free percent or
GB, average load, or a script's exit code) and a sentence describing it. Its
state is `open`, `acknowledged` or `resolved`; acknowledging and resolving
record who and when, and a resolution says why: `cleared` (the condition
passed), `remediated` (it passed just after a
[self-healing](monitors.md#self-healing) attempt succeeded), `manual` (a
person) or `not_applicable` (the rule no longer covers the device, or devices
no longer get its check because its owner lost their access). A monitor's
alert also keeps the latest failure's `output`. An acknowledged alert still
resolves itself when its condition passes. A person can resolve any alert,
but if its condition still holds the rule raises a new one at the next
evaluation.

Resolved alerts are kept for 90 days (`alerts::RESOLVED_RETENTION_MS`), then
the maintenance task deletes them.

## Evaluation and restarts

`server/src/alerts/` holds it all. The engine's task
(`alerts::spawn`) takes in new resource readings every five seconds, as
often as a watched device reports, and
judges the device-state rules every 15 seconds (`EVALUATION_INTERVAL`), at
once after a rule is saved, and soon after a device reports an update or
application scan, unless the scan found what the last one did (the same
pending updates and restart pending, or the same packages): then there is
nothing new to judge. An evaluation covers the whole instance, so reports
don't each start one: after an evaluation, reports wait at least 2 seconds
(`MIN_REPORT_SPACING`) and at least five times as long as it took
(`REPORT_SPACING_FACTOR`), at most until the next regular one, and every
device that reported meanwhile is judged in the next. A small instance
shows a scan's alert within a couple of seconds; a large one spends at most
about a sixth of its time on evaluations that reports start. A finished
self-healing attempt waits the same way; a saved rule doesn't, since
someone is looking at it. Script failure rules are checked
when an Agent reports a run, and when the engine finds a run lost; install
failure rules when an install is reported failed or found lost.

The engine keeps what devices' scans found that update and application
rules judge (each pending update's category and when it was first seen,
since when the device must restart, each outdated package and since when),
while an enabled rule judges it, and reads it again only for the devices
that reported since the last evaluation. Ages are counted from the
evaluation's time, so an update that becomes old enough raises its alert
with no new scan. It reads every device's again when it starts, after a rule
that judges them is saved or deleted, and every 10 minutes
(`alerts::FACTS_REFRESH_INTERVAL`), so a change it wasn't told of shows within
that. Pins in the application library are read with each evaluation, since
they change without any device reporting. A deleted device's are dropped.

Unresolved alerts live in the database, keyed by a partial unique index
(`idx_alerts_unresolved`), so a restart neither forgets an alert nor notifies
about it again. What the engine keeps in memory starts over with the server:

- A device counts as offline from the first evaluation after the server
  starts. A restart therefore delays offline alerts by up to their duration
  instead of raising one for every device whose Agent hasn't reconnected yet.
- Load rules need a full window of new readings.
- An Agent that went offline to install an update raises no offline alert
  during the update grace period (10 minutes); after it, the time counts from
  the disconnect.

## Notification channels

A channel is turned on or off, and is one of:

- **Email**: one message to each chosen user and each user holding a chosen
  role (directly or through an identity provider group), except disabled
  accounts, through the server's SMTP settings (**Authentication → Email**).
  Without SMTP an email channel can't be created, existing ones can't
  deliver, and the website says so.
- **Webhook**: a JSON `POST` to a URL. With a signing secret, each request
  carries `X-MeshRMM-Signature: sha256=<hex>`, the HMAC-SHA256 of the
  `X-MeshRMM-Timestamp` header's value, a `.`, and the raw body, keyed with
  the secret. Check it, and that the timestamp is recent, before trusting a
  request. The server never shows the secret again; it is stored encrypted
  with the instance key. Redirects aren't followed.

A rule notifies its chosen channels when it raises an alert, or only once
its self-healing has failed if it says so, and also when the alert is
resolved if the rule says so and its channels heard about the alert. An
[automation policy](automation.md#failure-notifications) notifies its
channels when one of its jobs finishes with failures. A failed webhook delivery (no
answer, a 5xx, 408 or 429) is tried again after 10 seconds, a minute and five
minutes; retries waiting when the server stops are lost. Each channel keeps
when its last delivery happened and why it failed, if it did. **Send test**
delivers a test notification once, even to a channel that's off.

A webhook's body:

```json
{
  "event": "alert.raised",
  "delivery_id": "8f0c…",
  "sent_at": 1761100000000,
  "server": "https://rmm.example.com",
  "summary": "Critical: Servers offline on FILESRV-01",
  "text": "Critical: Servers offline on FILESRV-01",
  "alert": { "id": "…", "rule_name": "Servers offline", "severity": "critical", "condition": "device_offline", "device_id": "…", "device_name": "FILESRV-01", "company_name": "Acme", "state": "open", "value": 16.0, "detail": "Offline for 16 minutes", "raised_at": 1761100000000, "…": "…" }
}
```

`event` is `alert.raised`, `alert.resolved`, `alert.remediation_failed` or
`test`, and is also in the
`X-MeshRMM-Event` header, as `delivery_id` is in `X-MeshRMM-Delivery`. `text`
repeats the summary for chat services, such as Slack's and Mattermost's
incoming webhooks, that show that field.

An `alert.remediation_failed` also carries the alert's last self-healing
attempt as `last_attempt` (how it ended in `status`, `detail` and
`exit_code`, and what it acted on), and, when self-healing stopped rather
than ran out of attempts, why as `remediation_stopped`. Its email says the
same on a `Last attempt:` line.

```json
{
  "event": "alert.remediation_failed",
  "summary": "Critical: Web server on WEB-01 (self-healing failed)",
  "alert": { "id": "…", "rule_name": "Web server", "remediation_state": "failed", "remediation_attempts": 3, "…": "…" },
  "last_attempt": { "attempt": 3, "action": "restart_service", "target": "nginx", "status": "failed", "exit_code": null, "detail": "Restarted “nginx”, but it stopped again within 3 seconds (failed (failed)).", "started_at": 1761100300000, "finished_at": 1761100304000, "…": "…" }
}
```

Channels are generic: `notifications::dispatch` takes channel IDs and a
`Notification` (an event name, a summary, an email body and the webhook's
data), so other features can notify the same channels.

### Delivery

`notifications::dispatch` only queues: each channel has one queue and, while
something is in it, one worker, so an event that raises an alert on many
devices at once (a site's internet going down, say) costs a few connections
rather than one per device.

- **Order**: a channel's notifications are delivered in the order they were
  dispatched. A channel that is turned off or deleted drops what is waiting.
- **Bound**: a channel holds at most 10,000 notifications, waiting or being
  delivered. Beyond that new ones are dropped, the server logs it once, and
  the channel's last delivery error says how many were dropped (for example
  "25 notifications were dropped because 10000 were already waiting") until
  a later delivery replaces it.
- **Email**: a channel waits a second before sending, so a burst arrives
  together. Then it takes everything waiting, looks up the recipients once
  and sends over one SMTP connection. Up to 20 notifications go out as one
  email each, as usual; more than 20 become one digest for each recipient,
  titled with how many (`37 alerts raised on Acme IT`, or `37 notifications
  on Acme IT` when they differ) and listing each one's summary, up to 300
  and then how many more.
- **Webhooks**: still one `POST` per notification, with at most 4 open per
  channel; the rest wait. They start in order, so up to 4 may arrive out of
  order. A delivery waiting for its retry gives its place to the next one,
  so it arrives after those dispatched later.
- **Last delivery**: a channel's last delivery time and error are written
  once a batch, and at most once a second while a burst is delivered. The
  error is the latest failure in that time, if any.

## Permissions and audit

| Permission | Allows |
|---|---|
| `alerts.view` | Seeing alerts and rules, and the alert count |
| `alerts.respond` | Acknowledging and resolving alerts |
| `alerts.manage` | Creating, changing and deleting rules and notification channels, and testing channels |

The Technician role starts with `alerts.view` and `alerts.respond`. The
audit log records `alert_rule.create`, `.update` and `.delete`,
`notification_channel.create`, `.update`, `.delete` and `.test` (a webhook's
host only, since its URL may carry a token), `alert.acknowledge` and
`alert.resolve`, and, for monitors, `monitor.check` and `alert.remediate`.
A rule with self-healing, or with a script, ping, port, website, certificate
or event log check, also needs `scripts.run` to create, change or turn on,
and runs with its owner's access; turning it off or deleting it needs only
`alerts.manage`. See
[Whose access it runs with](monitors.md#whose-access-it-runs-with).

## API

| Route | |
|---|---|
| `GET /v1/alerts` | `state` (`active`, `open`, `acknowledged`, `resolved`, `all`), `severity`, `company` (an ID or `none`), `device`, `limit` (up to 1000); returns `alerts`, `counts` and `truncated` |
| `GET /v1/alerts/counts` | Unresolved counts: `open`, `acknowledged`, and open ones by severity |
| `GET /v1/alerts/{id}` | One alert |
| `POST /v1/alerts/{id}/acknowledge`, `/resolve` | |
| `GET`, `POST /v1/alert-rules`; `PUT`, `DELETE /v1/alert-rules/{id}` | A rule's devices are `target` (`all`, `companies` or `tags`) with `company_ids`, or `tag_ids` and `tag_match` (`any` or `all`). Its `threshold`, `threshold_unit`, `duration_minutes`, `nonzero_exit_fails`, `update_category` (`any` or a category) and `days` as its condition needs. An install failure alert carries `update_run_id`. An application alert's `subject` is the application's name. |
| `GET`, `POST /v1/notification-channels`; `PUT`, `DELETE /v1/notification-channels/{id}` | A missing `webhook_secret` keeps the saved one; `null` removes it |
| `POST /v1/notification-channels/{id}/test` | `{ "delivered": bool, "error": string or null }` |
| `GET /v1/notification-channels/recipients` | Users and roles an email channel can reach |

A rule's `monitor`, `remediation` and `notify_when`, and the monitor and
self-healing routes, are in [Monitors](monitors.md#api).

The event socket (`/v1/events`) sends users who hold `alerts.view` an
`alert_counts` message after each snapshot, and an `alert_upsert` (the alert
and the new counts) whenever an alert is raised or changes. After changes to
many alerts at once, such as deleting a rule, it sends `alert_counts` alone,
and the website reads its lists again. Monitor results arrive as
`monitor_update`.

## Tables

`alert_rules` (with `target` and `tag_match`) with `alert_rule_companies`,
`alert_rule_tags` and `alert_rule_channels`;
`alerts`, with `monitor_states` and `alert_remediations` ([Monitors](monitors.md#tables)); `notification_channels` with `notification_channel_users` and
`notification_channel_roles`.

## Tests

- `cargo test -p meshrmm-server --lib` covers rule
  validation, the judging of each condition, and webhook bodies and
  signatures.
- `cargo test -p meshrmm-server --test alerts` runs on SQLite and
  PostgreSQL: raising, acknowledging and resolving, notifications to a
  webhook receiver, restarts, company scope, script failures and lost runs,
  permissions, and email and webhook channels. `--test applications` covers
  the application rules.
- `npm test` in `dashboard` covers the alert filters and lists, the rule and
  channel forms, and alert events on the socket.
