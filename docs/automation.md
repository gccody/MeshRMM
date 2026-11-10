# Automation

Automation runs [toolbox](toolbox.md) scripts, sends library files, and
scans for and installs [operating system updates](patch-management.md) on
many devices at once. A **job** is one script run, file delivery, update scan
or update install on a set of devices: chosen devices, whole companies, the
devices with chosen [tags](devices.md#tags), or every device. A **policy** starts jobs by itself, on a schedule or on each
device that enrolls. Policies' jobs are ordinary jobs, so their history is
the same job list.

## Jobs

### Starting a job

- **Devices page:** tick the devices (a tile's checkbox shows on hover, and on
  every tile once one is ticked), then choose **Run script** or **Send file**
  in the selection bar. **Select all N shown** ticks every device the
  filters show.
- **Toolbox:** **Run** on a script, or **Run on devices** at the top of the
  Scripts tab; **Send** on a library file.
- **Updates page:** tick devices, then **Scan now** or **Install updates…**
  (the categories to install, or all, and the restart); see
  [patch management](patch-management.md#the-website).
- **Applications page:** **Install on devices** on a library application,
  or the **Outdated** tab's **Update on their devices…** and **Update
  all…**; see
  [application deployment](application-deployment.md#installing-updating-and-uninstalling).

The dialog chooses:

- the script and the account it runs as (the signed-in user or
  SYSTEM/root), or the file and where it goes (the signed-in user's Documents
  or Public Documents, as in [the toolbox](toolbox.md#files));
- the devices: chosen ones, every device in chosen companies, every device
  with any (or all) of chosen tags, or every device. Companies, tags and
  every device need `jobs.run_wide`. They are resolved when the job starts,
  so a device that enrolls or is tagged later isn't part of it;
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
| Succeeded | The script exited with code 0, the file was saved, the scan finished, every chosen update installed, or no application failed. |
| Failed | The script couldn't run or exited with another code, the file wasn't saved, the scan failed, an update didn't install, or an application failed (the detail says how many, and the job's page lists which and why). |
| Timed out | The Agent stopped the script at its timeout. |
| No result | The device never reported: two minutes after the script's timeout, 30 minutes after a delivery started, 45 minutes after a scan, five hours after an install, or five hours and 15 minutes after an application run. |
| Skipped | The device has no interpreter for the script: PowerShell and Command Prompt run on Windows only, shell scripts on macOS and Linux only. Files go to every platform. An update job skips devices whose last scan found no update manager MeshRMM supports. An application job skips devices on a platform none of its applications is for, devices whose last scan found none of the package managers its applications install with, and devices where every application was skipped (pinned, or nothing the device can use); the detail gives the reason, and no alert is raised. |
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

The server starts queued devices a batch every second, while their kind's
class has fewer runs awaiting a report than its ceiling, and while the
device has fewer than two of its own. Both limits grow with the Agents
connected to the server: a device per second for every 100 connected, and
a run in flight of each class for every 4, never fewer than 25 a second and
500 in flight (what a small server has always had) and never more than 250
a second and 5,000 in flight:

| Agents connected | Started a second | In flight, each class |
|---|---|---|
| 100 | 25 | 500 |
| 1,000 | 25 | 500 |
| 10,000 | 100 | 2,500 |
| 100,000 | 250 | 5,000 |

A thousand-device job on a small server therefore reaches its devices over
about 40 seconds, a job on 100,000 devices over about seven minutes, and
their results arrive spread out rather than at once. The two classes are
**quick** (scripts, file deliveries and update scans) and **long** (update
installs and application installs, updates and uninstalls), each with its
own ceiling, so a fleet-wide install that fills its class leaves the quick
class's slots for a script run on one device. The per-device limit stays
well under the eight scripts an Agent runs at a time, leaving room for runs
started by hand. A run or delivery counts against the limits until it
reports or has no result.

Each second's batch takes devices from the queued jobs in turn: one from
every job, then a second from every job, and so on, and in each round the
jobs with fewer devices left to start go first, the older first among
equals. So a job on a few devices starts within a second or two even
behind a fleet-wide one, and gets the first slot its class frees; within a
job, devices start in the same order as ever.

A wake job's devices hold no run, so they don't count against either
ceiling: a queued wake job's devices are woken on the next pass (up to a
batch of them a second, besides the batch of runs) however full the
dispatcher is.

A queued device that can't start yet doesn't hold up the others: one at its
limit, one installing updates (for an update install job) or one running an
application install, update or uninstall (for an application job) is passed
over, and the devices queued after it start in its place. It starts once its
runs report, or have no result.

Waiting devices are queued as soon as their Agent connects. A device that
disconnects between being queued and being started waits again (or is
missed, if the job skips offline devices); a run that never reached its
Agent is removed rather than reported as failed.

The work for an Agent that connects is that device's alone: the server
queues its waiting parts by its ID, and Agents that connect together are
looked up in one statement and, when any of them waits, queued in one more,
so a fleet reconnecting after a server restart while a fleet-wide job waits
doesn't make the server reread the job for each of them. A job's counts, shown on its page and pushed to the website, are
recounted at most once a second, and only for the jobs whose devices
changed: those queued or started, and those whose devices reported. A job
is marked finished on the first recount after its last device reports, and
its waiting devices are missed within a few seconds of it expiring. Once a
minute, and when the server starts or a policy fires, the server also looks
at every waiting device and recounts every unfinished job, which catches
what the steps can't see: an Agent that connected while a policy was
creating its job, or a run that passed its report deadline without a report
(it shows as **No result**, and its job finishes, within a minute).

All of this state is in the database, so a server restart carries on with
queued and waiting devices. Runs that were on a device when the server
stopped report when it is back, or end up with no result.

### Creating a job

Creating a job reads much more than it writes: every device its target
covers (for every device, the whole fleet), each one's platform and update
support, and whether its Agent is connected. The server does all of that
first, through the database's readers, without holding SQLite's one writer
(`jobs::plan`), and then inserts the job in one transaction that writes and
reads nothing else (`jobs::write`): the job, its applications, its devices
400 to a statement, grouped by how they start, and the audit event. So
starting a job on 100,000 devices holds up other writes only for its
inserts, and the job still appears whole, since the dispatcher and the
job's counts read only committed rows. The website's own reads for a job
(the script, the file, the applications) and a policy's (its definition,
its owner's access, its script) come before the transaction too.

Things can change between planning and writing:

- A device deleted in between is left out: each insert takes only the
  devices that are still enrolled and not deleted. The job's counts and its
  `job.start` audit event say what was written, and a job on chosen devices
  is labelled by those left. If none is left, the job isn't created and
  fails with `no_devices`, as one planned with none does.
- An Agent that connects in between waits in the job, and is queued in a
  moment: the step after a job is created queues its waiting devices whose
  Agents are connected, a policy's firing leads to a full pass, and the
  minute's full pass catches any other. One that disconnects in between is
  queued, and when its turn comes the dispatcher finds it offline and makes
  it wait again (or misses it, if the job skips offline devices), as for one
  that disconnects later.
- A script edited in between is as one edited just after the job started:
  the job keeps the body it read.

A job on fewer than 1,000 devices, or on fewer devices than are connected,
asks whether each of its own devices is connected; a larger one copies the
connected devices once.

## Policies

**Jobs → Policies** lists the policies, their schedules, the next and last
runs (with the last job's result), and any problem that stopped the last
run. A policy has:

- a name and an **On** switch;
- what it does: run a **shared** script as an account; scan for updates;
  install the pending updates of chosen categories (or all), restarting
  never, if required or always, after a delay (see
  [patch management](patch-management.md#installing)); install chosen
  library applications, or update outdated applications: all of them, only
  chosen ones, or all but chosen ones (see
  [application deployment](application-deployment.md#policies)); or wake the devices
  that are offline (see [Wake-on-LAN](wake-on-lan.md#outcomes)), which
  skips online devices, never waits for offline ones, and can't run on
  enrollment;
- its devices: every device, chosen companies, the devices with any (or
  all) of chosen tags, or chosen devices. Companies and tags are resolved
  each time it runs, so a device tagged after the policy was saved is
  covered from its next run. A policy whose tags were all deleted covers no
  devices, and says so as its problem when it next fires;
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
| When a device enrolls | On each device that newly enrolls into its companies, or anywhere for every device. A device repaired with an installer again (keeping its ID) doesn't count. A new device has no tags yet, so these policies can't target tags or chosen devices. An **Install applications** policy also runs on each newly enrolled device it covers, whatever its trigger. |

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

A recurring policy doesn't run while its earlier jobs still have devices
queued: online, but not started yet because the dispatcher hasn't reached
them or they are at their limits. That time is skipped instead, the
policies page says so ("Skipped its run at … because 3 devices from its
last run hadn't started yet") and the audit log records
`automation_policy.skip`; the next time runs as usual once they have
started. So a policy on a short interval over a large fleet runs as often as
its jobs can be handed out, rather than piling up jobs faster than they
start, and every device gets each run it does start. **Run now** always
starts a job.

### Whose access a policy runs with

A policy runs with the access of the user who last saved it, shown as its
owner. Saving one needs `automation.manage` and what it does needs:
`scripts.run` for a script, `updates.view` for a scan, `updates.install` for
an install, `devices.wake` for a wake, `applications.deploy` for installing or
updating applications. When it is due, its owner must still be enabled and hold both
permissions; otherwise it
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
firing, or two passes at once, can't run a policy twice. The job is planned
just before that transaction (see [creating a job](#creating-a-job)), and the
claim also needs the policy unchanged since: one saved, turned off or
deleted in between starts nothing from what it was, and a saved one is
planned again from what was saved.

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
| `updates.view`, `updates.install` | Scan and install [update](patch-management.md) jobs on chosen devices, and seeing one's own jobs. | Yes |
| `applications.view`, `applications.deploy` | Seeing one's own [application](application-deployment.md) jobs; install, update and uninstall jobs on chosen devices. | Yes |
| `jobs.run_wide` | Running scripts, sending files and updating on whole companies, tags or every device. | No |
| `automation.manage` | Seeing, creating, changing and deleting policies, and seeing their jobs. With what a policy does needs (`scripts.run`, `updates.view`, `updates.install`, `devices.wake` or `applications.deploy`), saving and running it. | No |
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
| `POST /v1/jobs` | `{script_id, run_as}`, `{file_id, destination}` or `{updates: {action: "scan"}}` / `{updates: {action: "install", categories, reboot, reboot_delay_minutes}}` or `{applications: {action, application_ids, packages, mode, inventory}}` (see [application deployment](application-deployment.md#api)), with `target` (`devices`, `companies`, `tags` or `all`), `device_ids`, `company_ids` or `tag_ids` with `tag_match` (`any`, the default, or `all`), `offline` (`skip` or `wait`) and `wait_hours`. Returns the job with its devices. `400 no_devices` if the target covers none. |
| `GET /v1/jobs?policy_id=&limit=` | Jobs the user may see, newest first, with counts (50 by default, at most 200). |
| `GET /v1/jobs/{id}` | A job with `devices`: each one's `outcome`, `detail`, `run_id`, `delivery_id`, `update_run_id`, `application_run_id` or `wake_id`, exit code, saved path and times, and for an application job `applications`: `[{name, outcome, error}]` for those that failed or were skipped. |
| `POST /v1/jobs/{id}/cancel` | Cancels the devices that haven't started. |
| `GET /v1/automation-policies` | Policies with their latest job (`automation.manage`). |
| `POST /v1/automation-policies`, `PUT /v1/automation-policies/{id}` | A whole policy: `name`, `enabled`, `action` (`script`, `update_scan`, `update_install`, `wake`, `app_install` or `app_update`), `script_id` and `run_as` for a script, `update_categories`, `reboot` and `reboot_delay_minutes` for an install, `applications` (`{application_ids, packages, mode}`) for an application policy, `target` with `device_ids`, `company_ids` or `tag_ids` and `tag_match`, `trigger` (`interval`, `daily`, `weekly`, `once`, `enroll`) with `interval_minutes`, `time` (`HH:MM`), `weekdays` (1 = Monday to 7), `run_at` (`YYYY-MM-DDTHH:MM`) and `time_zone` as it needs, `offline`, `catch_up_hours` and `channel_ids`. |
| `DELETE /v1/automation-policies/{id}` | |
| `POST /v1/automation-policies/{id}/run` | Run now; returns the job. |

A job reads `{id, kind, item_id, item_name, language, run_as, destination,
categories, reboot, reboot_delay_minutes, target, target_label, offline, expires_at_unix_ms, requested_by,
requested_by_you, policy_id, policy_name, created_at_unix_ms,
finished_at_unix_ms, can_cancel, counts}`, where `counts` has `total` and one
number per outcome. The event socket (`/v1/events`) sends
`{"type": "job_update", "job": …}` with each job a user may see whose counts
change, and `{"type": "jobs_resync"}` when the user fell too far behind and
should read the lists again.

## How it's built

- `server/src/automation/`: `jobs.rs` (planning and writing jobs, outcomes
  and counts, who sees what), `dispatch.rs` (expiry, queueing connected devices, starting
  runs and deliveries within the limits), `policies.rs` (policies, firing,
  enrollment), `schedule.rs` (triggers and time zones, with `chrono-tz`),
  and `mod.rs` (the engine's task, publishing changes, finishing jobs and
  failure notifications). The engine steps every 5 seconds, every second
  while queued devices could start (the last pass stopped at its batch,
  not at a class's ceiling, which only a report frees), and at once when
  told what changed: an Agent
  connected (`device_connected`), a device's run reported
  (`device_changed`), or a job was created or cancelled (`job_changed`). A
  step queues the connected devices' waiting parts by device ID and starts
  queued devices; at most once per `SETTLE_SPACING_MS` (a second) it
  misses the waiting devices of jobs that expired since the last and
  recounts the jobs it was told of. A full pass, every
  `RECONCILE_INTERVAL_MS` (a minute), after a restart and when a policy
  fires, also queues every waiting device whose Agent is connected, misses
  deleted devices' parts (deleting a device misses them at once; this is
  for a server that stopped in between) and recounts every unfinished job.
  The dispatcher's limits come from `dispatch::limits` with the count of
  connected Agents (`AgentHub::count`); its floors and caps are the
  constants `MIN_BATCH`, `MAX_BATCH`, `MIN_IN_FLIGHT` and `MAX_IN_FLIGHT`.
  The caps keep the starts within what SQLite's single writer takes
  alongside everything else: 250 starts a second, each a transaction of a
  few statements, and their reports later, are about 500 small write
  transactions a second. A pass counts the runs in flight per class in the
  same four queries as before (update runs grouped by kind too), and reads
  the queued parts in the order above with `ROW_NUMBER()` and `COUNT(*)`
  windows over each job's startable parts.
- `server/src/api/jobs.rs` and `server/src/api/automation.rs`.
- Targets: `server/src/targets.rs` resolves every kind of target (chosen
  devices, companies, tags, every device) for jobs, policies and
  [alert rules](alerts.md) alike: as an SQL condition when a job is
  created, or against one device's company and tags when a policy or rule
  checks it.
- Tables: `jobs` (what it sends, its target and expiry, who or which policy
  started it), `job_devices` (each device's state), `automation_policies`
  (with `tag_match`) and `automation_policy_companies`, `_devices`, `_tags`
  and `_channels`. Runs,
  deliveries and update runs carry `job_id`; a job's runs have the source
  `job`. An update job's kind is `update_scan` or `update_install`, its
  `item_name` says what it does, and its `item_id` is empty. A wake job's
  kind is `wake`; its devices are woken together by the dispatcher, read
  apart from the other jobs' and outside their limits, and their outcomes
  come from `wake_requests`. An application
  job's kind is `app_install`, `app_update` or `app_uninstall`; what it does
  is kept in `application_jobs`, a device's part waits while the device runs
  another application run, and its outcome comes from `application_runs`.
- Each device's part is the same `run_script`, `deliver_file`,
  `scan_updates`, `install_updates` or `run_applications` command as one
  started by hand.
- The website: `dashboard/features/automation/`.

## Tests

- `cargo test -p meshrmm-server automation` runs the unit tests for
  schedules, outcomes and policy checks.
- `cargo test -p meshrmm-server --test automation`, on SQLite and
  PostgreSQL: jobs across platforms with skips, waiting and expiry,
  cancelling, the dispatcher's limits (growing with the connected Agents,
  each class's ceiling, small jobs beside large ones, wakes however full),
  file jobs, policies firing once across a restart, skipping a run while
  the last one's devices are queued, missed times, owners' access,
  enrollment policies, alerts and failure notifications, and retention;
  and that creating a job plans it without the writer and then only
  inserts, leaves out devices deleted in between, and starts nothing from a
  policy changed between planning its firing and claiming it.
- `npm test` in `dashboard` covers the job and policy models.
