# Automation

Automation runs [toolbox](toolbox.md) scripts and sends library files on many
devices at once. A **job** is one script run or file delivery on a set of
devices: chosen devices, whole companies, or every device. A **policy** starts
jobs by itself, on a schedule or on each device that enrolls. Policies' jobs
are ordinary jobs, so their history is the same job list.

## Jobs

### Starting a job

- **Devices page:** tick the devices (a tile's checkbox shows on hover, and on
  every tile once one is ticked), then choose **Run script** or **Send file**
  in the selection bar. **Select all N shown** ticks every device the
  filters show.
- **Toolbox:** **Run** on a script, or **Run on devices** at the top of the
  Scripts tab; **Send** on a library file.

The dialog chooses:

- the script and the account it runs as (the signed-in user or
  SYSTEM/root), or the file and where it goes (the signed-in user's Documents
  or Public Documents, as in [the toolbox](toolbox.md#files));
- the devices: chosen ones, every device in chosen companies, or every
  device. Companies and every device need `jobs.run_wide`. They are resolved
  when the job starts, so a device that enrolls later isn't part of it;
- what happens to devices that are offline: skip them, or run when they
  connect within 1 to 168 hours (24 by default).

It previews how many devices the job covers, how many will be skipped and how
many are offline, then opens the job's page. A device's **Scripts** button
still runs a script on that one device and shows its output in place.

The job keeps the script's body, interpreter and timeout as they were when it
started, so every device runs the same script even if someone edits it while
the job waits for offline devices. A file job keeps the file's ID; a device
whose turn comes after the file was deleted from the library fails.

### Each device's part

| Outcome | Meaning |
|---|---|
| Queued | Online; its run starts in a moment (see [dispatching](#dispatching)). |
| Waiting | Offline; it runs when its Agent connects, if the job hasn't expired. |
| Running | Started; the device hasn't reported yet. |
| Succeeded | The script exited with code 0, or the file was saved. |
| Failed | The script couldn't run or exited with another code, or the file wasn't saved. |
| Timed out | The Agent stopped the script at its timeout. |
| No result | The device never reported: two minutes after the script's timeout, or 30 minutes after a delivery started. |
| Skipped | The device has no interpreter for the script: PowerShell and Command Prompt run on Windows only, shell scripts on macOS and Linux only. Files go to every platform. |
| Missed | It was offline and the job skips offline devices, or it didn't connect before the job expired, or a newer run of the same policy replaced the wait, or the device was deleted. The detail says which. |
| Cancelled | Someone cancelled the job before the device started. |

Mixed fleets are expected: a device without the interpreter is skipped and
counted as such, not failed. A job is finished once no device is queued,
waiting or running.

### The job pages

**Jobs** in the navigation lists recent jobs, newest first, with what each
sends, its devices, who or which policy started it, a progress bar and its
counts. `?policy=<id>` lists one policy's jobs. A job's page
(`/job?id=<id>`) shows the counts, which filter the device list when chosen,
every device's outcome and detail, and **View output** for a device's run.
Both pages update live from the website's event socket, and the job page
re-reads itself every 10 seconds while the job runs in case an update was
missed.

**Cancel the rest** cancels the devices that haven't started. Scripts already
running on a device finish; the Agent can't stop them early.

Users see the jobs they started, users with `automation.manage` see every
policy's jobs, and users with `audit.view` see everyone's. Whoever may see a
job may read its runs' output. The toolbox's **Run history** lists runs
started on one device; a job's runs are on the job's page and in a device's
own run list (`GET /v1/script-runs?device_id=`).

### Dispatching

The server starts queued devices in the order their jobs were created, at
most 25 every second, while fewer than 500 jobs' runs and deliveries await a
report across the server, and while the device has fewer than two of its
own. A thousand-device job therefore reaches its devices over about 40
seconds, and their results arrive spread out rather than at once. The
per-device limit stays well under the eight scripts an Agent runs at a time,
leaving room for runs started by hand. A run or delivery counts against the
limits until it reports or has no result.

Waiting devices are queued as soon as their Agent connects. A device that
disconnects between being queued and being started waits again (or is
missed, if the job skips offline devices); a run that never reached its
Agent is removed rather than reported as failed.

All of this state is in the database, so a server restart carries on with
queued and waiting devices. Runs that were on a device when the server
stopped report when it is back, or end up with no result.

## Policies

**Jobs → Policies** lists the policies, their schedules, the next and last
runs (with the last job's result), and any problem that stopped the last
run. A policy has:

- a name and an **On** switch;
- a **shared** script and the account it runs as;
- its devices: every device, chosen companies, or chosen devices;
- when it runs (below);
- what happens to offline devices: skip them, or catch them up when they
  connect within 1 to 168 hours (24 by default);
- notification channels told when one of its jobs finishes with failures.

**Run now** starts a job of the policy at once, as the user who chose it,
without changing its schedule.

### When a policy runs

| Trigger | Runs |
|---|---|
| Every N minutes or hours | Every 5 minutes to 28 days, counted from when the policy was last saved or turned on. The first run is one interval after that. |
| Daily | At a time of day in a chosen IANA time zone. |
| Weekly | At a time of day on chosen days of the week, in a time zone. |
| Once | At a date and time in a time zone, which must be in the future when it's saved. It stays on with nothing left to do afterwards. |
| When a device enrolls | On each device that newly enrolls into its companies, or anywhere for every device. A device repaired with an installer again (keeping its ID) doesn't count. |

Times of day follow the zone's clock, so 03:00 stays 03:00 across daylight
saving changes. A time the clocks skip that day runs at the first minute
after the change; a time that happens twice runs the first time.

A new device is waited for even if the policy skips offline devices, because
it connects moments after enrolling; it gets the policy's catch-up time. An
enrollment policy can't choose devices, since new devices have none chosen.

When a recurring policy runs again, its earlier jobs stop waiting for offline
devices (they are missed: "the policy ran again before the device
connected"), so a device that was away for several runs catches up once, not
once per run.

### Whose access a policy runs with

A policy runs with the access of the user who last saved it, shown as its
owner. Saving one needs `automation.manage` and `scripts.run`. When it is
due, its owner must still be enabled and hold both permissions; otherwise it
doesn't run, records why, and shows it on the policies page. Saving the
policy again makes the saver its owner and clears the problem.

Policies run shared scripts only. A private script is visible to its owner
alone, so other administrators couldn't see what a policy ran. While a policy
uses a script, the script can't be deleted or unshared
(`409 script_in_use`).

### Restarts and missed times

Firing claims the policy's due time and creates the job in one database
transaction: the next time is written only together with the job, and only
if the due time is still the one the pass read. A restart in the middle of a
firing, or two passes at once, can't run a policy twice.

When the server was down at a policy's time:

- At most an hour late, the run happens when the server starts.
- Later than that, an interval policy runs once when the server starts and
  then continues on its interval. Daily, weekly and once policies skip the
  missed time, since a script meant for 03:00 could disrupt people at noon;
  the policies page says it was missed ("Missed its run at … because the
  server wasn't running") and the audit log records
  `automation_policy.skip`.
- However long the downtime, a missed time runs at most once.

A policy that is turned off doesn't run; turning it back on starts its
schedule from then, without catching up.

### Failure notifications

When a policy's job finishes and any device failed, timed out or had no
result, the policy's [notification channels](alerts.md#notification-channels)
get a notification with the event `job.failed`: a summary such as
"“Nightly backup”: 3 of 20 devices failed", the counts, up to 20 failed
devices with their exit code or error, and a link to the job. A webhook's
body carries the job as `job`.

## Alerts

Each device's part of a job is an ordinary toolbox run, so
[Script failed](alerts.md#rules) rules raise alerts for job runs that fail,
time out or never report, like runs started by hand.

## Permissions

| Permission | Allows | Technician |
|---|---|---|
| `scripts.run` | Running scripts on chosen devices, and seeing one's own jobs. | Yes |
| `files.deliver` | Sending files to chosen devices. | Yes |
| `jobs.run_wide` | Running scripts and sending files on whole companies or every device. | No |
| `automation.manage` | Seeing, creating, changing and deleting policies, and seeing their jobs. With `scripts.run`, saving and running them. | No |
| `audit.view` | Seeing everyone's jobs and runs. | No |

Administrators have every permission. Grant `jobs.run_wide` and
`automation.manage` to a custom role to let technicians use them.

The audit log records each job once, not each device's run:

- `job.start`: who started it (a user, or `policy “<name>”` with the owner's
  ID in the metadata), what it sends, its target and how many devices were
  queued, waiting and skipped.
- `job.cancel`;
- `automation_policy.create`, `.update`, `.delete`, `.run` (Run now) and
  `.skip` (a due run that didn't happen, with the reason).

## Retention

Jobs and their devices are kept for 30 days, like toolbox runs and their
output, then the maintenance task deletes them. Policies are kept until
someone deletes them; their jobs stay in the history after that.

## API

All routes need a signed-in user.

| Route | |
|---|---|
| `POST /v1/jobs` | `{script_id, run_as}` or `{file_id, destination}`, with `target` (`devices`, `companies` or `all`), `device_ids` or `company_ids`, `offline` (`skip` or `wait`) and `wait_hours`. Returns the job with its devices. `400 no_devices` if the target covers none. |
| `GET /v1/jobs?policy_id=&limit=` | Jobs the user may see, newest first, with counts (50 by default, at most 200). |
| `GET /v1/jobs/{id}` | A job with `devices`: each one's `outcome`, `detail`, `run_id` or `delivery_id`, exit code, saved path and times. |
| `POST /v1/jobs/{id}/cancel` | Cancels the devices that haven't started. |
| `GET /v1/automation-policies` | Policies with their latest job (`automation.manage`). |
| `POST /v1/automation-policies`, `PUT /v1/automation-policies/{id}` | A whole policy: `name`, `enabled`, `script_id`, `run_as`, `target` with `device_ids` or `company_ids`, `trigger` (`interval`, `daily`, `weekly`, `once`, `enroll`) with `interval_minutes`, `time` (`HH:MM`), `weekdays` (1 = Monday to 7), `run_at` (`YYYY-MM-DDTHH:MM`) and `time_zone` as it needs, `offline`, `catch_up_hours` and `channel_ids`. |
| `DELETE /v1/automation-policies/{id}` | |
| `POST /v1/automation-policies/{id}/run` | Run now; returns the job. |

A job reads `{id, kind, item_id, item_name, language, run_as, destination,
target, target_label, offline, expires_at_unix_ms, requested_by,
requested_by_you, policy_id, policy_name, created_at_unix_ms,
finished_at_unix_ms, can_cancel, counts}`, where `counts` has `total` and one
number per outcome. The event socket (`/v1/events`) sends
`{"type": "job_update", "job": …}` with each job a user may see whose counts
change, and `{"type": "jobs_resync"}` when the user fell too far behind and
should read the lists again.

## How it's built

- `server/src/automation/`: `jobs.rs` (creating jobs, outcomes and counts,
  who sees what), `dispatch.rs` (expiry, queueing connected devices, starting
  runs and deliveries within the limits), `policies.rs` (policies, firing,
  enrollment), `schedule.rs` (triggers and time zones, with `chrono-tz`),
  and `mod.rs` (the engine's task, publishing changes, finishing jobs and
  failure notifications). The engine makes a pass every 5 seconds, every
  second while devices are queued, and at once when a job is created, an
  Agent connects or a device reports.
- `server/src/api/jobs.rs` and `server/src/api/automation.rs`.
- Tables: `jobs` (what it sends, its target and expiry, who or which policy
  started it), `job_devices` (each device's state), `automation_policies`
  with `automation_policy_companies`, `_devices` and `_channels`. Runs and
  deliveries carry `job_id`; a job's runs have the source `job`.
- The Agent needs nothing new: each device's part is the same `run_script`
  or `deliver_file` command as a run started by hand.
- The website: `dashboard/features/automation/`.

## Tests

- `cargo test -p meshrmm-server automation` runs the unit tests for
  schedules, outcomes and policy checks.
- `cargo test -p meshrmm-server --test automation`, on SQLite and
  PostgreSQL: jobs across platforms with skips, waiting and expiry,
  cancelling, the dispatcher's limits, file jobs, policies firing once across
  a restart, missed times, owners' access, enrollment policies, alerts and
  failure notifications, and retention.
- `npm test` in `dashboard` covers the job and policy models.
