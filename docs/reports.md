# Reports

MeshRMM makes reports about the fleet on demand and on a schedule: an
executive summary for a customer's monthly review, patch compliance for an
audit, and hardware, software and alert reports. Each is one self-contained
HTML page that opens in a browser tab, prints or saves as PDF with a page
per section, and downloads every table as CSV.

Reports are on the **Reports** page, for users with `reports.view`.

## Templates

| Template | What it shows | Period |
|---|---|---|
| Executive summary | Devices by platform and by operating system and version; how many are online now and which haven't been seen in N days (30 unless you choose); alerts raised and resolved in the period by kind, the mean time to resolve them and how many are open now; patch posture (fully patched devices, devices with security or critical updates pending, failed installs in the period, restarts pending); application posture (devices with outdated applications, the applications outdated on the most devices, failed application installs and updates); jobs and policy runs that succeeded or failed; the devices with the most alerts | Yes |
| Patch compliance | For each device, its last scan, its pending updates by severity and category, failed installs in the period and whether a restart is pending; totals across the devices and the share that is compliant | Yes |
| Hardware asset inventory | For each device: company, platform, manufacturer, model, serial number, processor, memory, disks, storage used and free, operating system and version, Agent version and when it was last seen; with counts by model and operating system | No: as the devices are now |
| Software inventory | The applications on the devices, with how many devices have each and each version; the packages their package managers can update | No: as the devices are now |
| Alerts | Alerts raised in the period by kind, by rule and by device, with how many are still open, how long the resolved ones took on average and at most, and each alert | Yes |

**Compliance.** A device is compliant when its update manager is one
MeshRMM supports, its last scan worked, that scan is at most 7 days old and
it found no security or critical updates. Devices whose operating system
MeshRMM can't patch are listed as *Unsupported* and left out of the
percentage; devices that have never been scanned count as not compliant.
Pending updates of other kinds (drivers, feature packs) and pending restarts
are shown but don't make a device non-compliant.

**Last seen.** An online device is seen now. Otherwise it's the latest of
its last resource reading, its last system information snapshot and its
enrollment.

## Scope and period

A report covers **every device**, the devices of **chosen companies**, or
the devices with **chosen tags** (any or all of them), as they are when the
report is made. These are the same choices as jobs, policies and alert rules.

The period is the **last 7 days** or **last 30 days** (whole days, up to
and not including today), the **previous calendar month**, or a **custom
range** of up to 366 days that ends today at the latest. Days start at
midnight in the report's **time zone**, which the run dialog sets to the
browser's; the report shows its times in that zone too. A schedule judges
its period when it runs, so a monthly schedule with *previous calendar
month* covers the whole month before each run.

### What the server no longer has

Reports count only what the server still keeps, and each report says so
under *Coverage* instead of showing a smaller number as if it were whole:

| History | Kept |
|---|---|
| Resolved alerts | 90 days |
| Jobs and script runs | 30 days |
| Update installs | 365 days |
| Application installs, updates and uninstalls | 365 days |
| Resource readings (used for free space and last seen) | 7 days |

A period that starts before what is kept gets a note such as *“Resolved
alerts are kept 90 days, so this report counts them from 12 Aug 2026
only.”* Pending updates, outdated packages, hardware and applications are
the devices' current state, and the reports say that too: they can't say
what was pending on a past day. Deleted devices and rules aren't counted.

## Running a report

Choose **Run** on a template, then the devices, the period and the time
zone. The server makes the report in the background, one at a time, off the
request path; the list shows it as *Waiting* and *Making* and then *Ready*.
**Open** shows it in a new tab, the download button saves the HTML, and
each table's **Download CSV** button saves that table. A user may have 5
reports waiting at once.

Making a report only reads: it runs aggregate queries on the database's
readers, outside any transaction, so it never holds SQLite's single writer.
Its only writes are marking it started and finished.

Reports are kept for **90 days** and then deleted by the server's
housekeeping, with their files. Users with `reports.manage` may delete one
sooner.

### The page

The HTML page has no external requests: its styles and the small script that
builds CSV files are inline, and the server sends it with a Content Security
Policy that allows only those two by hash. It follows the browser's light or
dark theme, and prints with each section on a new page, the table headers
repeated and the buttons hidden: use the browser's *Print → Save as PDF* for
a PDF. It shows the instance name, the devices it covers, the period and
time zone, and when it was made. Tables hold at most 10,000 rows; a larger
one says how many it left out.

## Schedules

Users with `reports.manage` schedule reports on the **Schedules** tab:

- **When**: daily, weekly on a weekday, or monthly on a day from 1 to 28, at
  a time in a time zone.
- **What**: the template, devices and a period (last 7 or 30 days, or the
  previous month).
- **Who**: users of the server, and other email addresses, up to 50. Each
  gets their own email with a summary of the first section, a link to the
  report and the page attached when it is under 15 MiB.

A schedule runs with the access of the user who last saved it, as policies
do. If that user is disabled or no longer holds `reports.view` and
`reports.manage`, the schedule records a failed report saying so, instead of
running with access nobody has.

**Run now** makes the schedule's report at once, for the period that ends
today, and emails it; the schedule's next run doesn't change. A schedule
that is turned off doesn't run.

Email goes through the server's SMTP settings (*Authentication → Email*).
When those aren't set, the Reports page says so, and schedules still make
their reports and keep them, marked *Not emailed*. A report whose email
failed shows why.

## Permissions and audit

| Permission | Allows |
|---|---|
| `reports.view` | Seeing the templates, schedules and reports, running reports, and opening and downloading them |
| `reports.manage` | Creating, changing, running and deleting schedules, choosing their recipients, and deleting reports |

No built-in role except Administrator starts with them.

The audit log records `report.generate` for every report, with its
template, devices, period and time zone and what started it: a user (or
their API token), a schedule (`Report schedule “Name”`) or *Run now*. It
also records `report.delete`, and `report_schedule.create`, `.update` and
`.delete`.

## API

Reports are reachable with [API tokens](api.md#api-tokens) that carry the
permissions above.

| Route | |
|---|---|
| `GET /v1/report-templates` | The `templates` (`id`, `title`, `description`, `uses_period`, `uses_stale_days`), `email_configured`, `retention_days`, `default_stale_days`, `max_stale_days` and `max_custom_days` |
| `GET /v1/reports` | Newest first: `limit` (up to 500), `before` (a `created_at`), `schedule_id`; returns `reports` and `next_before` |
| `POST /v1/reports` | `template`, `target` (`all`, `companies` or `tags`) with `company_ids`, or `tag_ids` and `tag_match`; `period` (`last_7_days`, `last_30_days`, `previous_month` or `custom` with `from` and `to` as `YYYY-MM-DD`), `time_zone` (IANA) and `stale_days`. Answers `202` with the queued report, or `409` `reports_busy` |
| `GET /v1/reports/{id}` | One report: `status` (`queued`, `running`, `succeeded`, `failed`), `error`, `trigger`, `period_start` and `period_end` (Unix ms), `delivery` (`pending`, `sent`, `failed`, `not_configured`, or `null` with no recipients) and, once it succeeded, its `tables` |
| `GET /v1/reports/{id}/html` | The page; `?download` saves it. `409` `report_not_ready` until it succeeded |
| `GET /v1/reports/{id}/tables/{table}.csv` | One table as CSV |
| `DELETE /v1/reports/{id}` | `reports.manage` |
| `GET`, `POST /v1/report-schedules`; `PUT`, `DELETE /v1/report-schedules/{id}` | `name`, `enabled`, `template`, the devices as above, `period`, `stale_days`, `frequency` (`daily`, `weekly` with `weekday` 1–7 from Monday, `monthly` with `day_of_month`), `time` (`HH:MM`), `time_zone`, `user_ids` and `emails` |
| `GET /v1/report-schedules/recipients` | The users a schedule can email |
| `POST /v1/report-schedules/{id}/run` | Run now; answers `202` with the queued report |

```sh
# Last month's patch compliance for one company, in New York time.
id=$(curl -fsS -H "Authorization: Bearer $MESHRMM_TOKEN" -H "Content-Type: application/json" \
  -d '{"template": "patch_compliance", "target": "companies", "company_ids": ["<company id>"],
       "period": "previous_month", "time_zone": "America/New_York"}' \
  "$MESHRMM/v1/reports" | jq -r .id)
# Poll until it's made, then save the page and one table.
curl -fsS -H "Authorization: Bearer $MESHRMM_TOKEN" "$MESHRMM/v1/reports/$id" | jq '{status, error}'
curl -fsS -H "Authorization: Bearer $MESHRMM_TOKEN" -o compliance.html "$MESHRMM/v1/reports/$id/html?download"
curl -fsS -H "Authorization: Bearer $MESHRMM_TOKEN" -o devices.csv "$MESHRMM/v1/reports/$id/tables/devices.csv"
```

## How it works

A report is a row in `reports` and, once made, a JSON document in the data
directory's `reports/` folder: the figures and tables, not HTML. The page
and the CSV files are rendered from it on each request, so a fix to the
page's layout applies to reports already made. The server's report task
claims queued reports one at a time, fires schedules that are due (claiming
each run with a conditional update, so one run makes one report), and sends
the emails of finished ones. When the server restarts, reports it was making
are queued again.

There is no PDF renderer in the server: a good PDF of tables needs a layout
engine, and browsers already have one. Printing the page gives a clean PDF.
