# Maintenance windows

A device in maintenance is one people are working on, or one that is about
to be disrupted on purpose: a disk swap, a reimage, a rack move, a patch
night. Its alerts would only be noise, and some automation would get in the
way, so while a **maintenance window** is in force on it:

- **Alerts are held back.** No [alert](alerts.md) is raised for it, no
  notification is sent and [self-healing](monitors.md#self-healing) waits.
- **Scheduled automation is held back**, as the window chooses. By default
  every [policy](automation.md#policies)'s scheduled run skips it: scripts,
  update scans and installs, application policies and wakes. A window can
  let update scans run while holding back everything else, or let all
  scheduled automation run and hold back only alerts.
- **People's own actions always work.** Remote sessions, terminals, files,
  scripts and jobs someone starts, and a policy's **Run now**, run as usual.
  The website shows the device's maintenance wherever they do.

Maintenance is not the same as the viewer's
[maintenance controls](maintenance-controls.md), which block input and black
out screens during one remote session.

## Windows

A window covers one device. It has:

- a **start**: now, or a time up to a year ahead;
- an **end**: a set time (at most a year after its start), or none, so it
  lasts until someone ends it;
- a **reason**, up to 500 characters, shown wherever the window is;
- what it lets run of the device's **scheduled automation**: `skip`
  (nothing, the default), `scans` (update scans only) or `run` (everything);
- who set it: a person, or a policy.

A device may have several windows, overlapping or not. It is in maintenance
while any of them is in force, and the strictest of those decides what
automation runs. A window that hasn't started yet is **upcoming**; ending it
**cancels** it. Ending a window in force ends it now; it is **ended** from
then on, as it is once its end passes.

Putting many devices in maintenance at once writes a window for each, so
each can be ended, changed or audited on its own.

## Starting and ending maintenance

In the website:

- **Maintenance** in the selection bar on the device wall puts the ticked
  devices in maintenance, and ends it on those already in maintenance.
- **Put in maintenance…** in a device's **⋯** menu on the wall, or **Put in
  maintenance** on its page.
- **Start maintenance** on the **Maintenance** page (under Companies) puts
  every device, chosen companies', devices with chosen tags or chosen
  devices in maintenance, now or later.
- A [maintenance policy](#maintenance-policies) does it on a schedule, and an
  [update install](#maintenance-while-installing) can do it for itself.

The start dialog takes the reason, when it starts, how long it lasts (a
length, until a time, or until someone ends it) and what scheduled
automation it lets run.

While a device is in maintenance:

- its tile on the wall carries a **Maintenance** badge;
- the wall's **Maintenance** filter, shown while any device is in
  maintenance, lists them (`/?status=maintenance`);
- its page shows a banner with the reason, who set it, when it ends and
  **End now**, which ends every window in force on it, and lists its
  upcoming windows with **Cancel**;
- its terminals and files pages say it is in maintenance.

The **Maintenance** page lists the windows in force and to come, or those
past, with the device, its company, when, why, who set and ended each, and
**End now** or **Cancel**. Everything follows the event socket: a window
starting or ending shows at once.

## What happens at the edges

**Alerts open when a window starts stay as they are.** Maintenance freezes a
device's alerts rather than clearing them: an open alert is neither
resolved by its condition clearing nor notified again while the device is in
maintenance, and its self-healing waits. People can still acknowledge and
resolve it. An alert whose rule changed so it no longer covers the device is
still resolved as not applicable.

**When a device leaves maintenance it is judged again at once**, not at the
next evaluation: a disk still full raises its alert then, an alert whose
problem went away resolves, and a device offline throughout raises its
offline alert, since how long it has been offline is counted all along.
Load rules (CPU, memory) judge the readings of their window, which run up to
the end; [monitors](monitors.md) judge the latest result each check
reported during maintenance, which was kept. Self-healing picks up where it
waited.

**Failures during maintenance are dropped, not saved for later.** A script
that fails, an update install that fails and an application install that
fails while the device is in maintenance raise nothing then or afterwards.
The one exception is a window an update install set for itself: that
install's own failure still alerts.

**A scheduled run due during a window is skipped, not queued.** The device
is recorded on the job as skipped with "In maintenance until …: *reason*"
(or "In maintenance: *reason*" for a window with no end), and the run
doesn't happen later. That covers a device in maintenance when the job is
planned and one that enters maintenance before a waiting job reaches it:
when it connects, the job skips it rather than starting. Runs people start,
and **Run now**, never skip a device for maintenance.

**Ends are kept exactly.** The server notices a window starting or ending
within a second or two (it wakes for the next start or end, and checks at
least every 15 seconds), republishes the device and, for a device leaving
maintenance, judges it again.

## Maintenance policies

A [policy](automation.md#policies) whose action is **Maintenance window**
uses the policies' schedules (every few hours, daily, weekly or once, at a
time in a time zone) to put its devices in maintenance for a set length, 5
minutes to 7 days: every Sunday from 02:00 to 05:00, say, is a weekly policy
on Sunday at 02:00 for 180 minutes. Its reason, if given, is the windows'
reason, otherwise the policy's name, and it chooses what automation they let
run like any window.

- Each run writes a window on each device the policy covers then, from its
  run to its due time plus the length, set by the policy. It starts no job;
  the policies list shows how many devices its last run put in maintenance
  and until when, and the windows show under **Maintenance**.
- A run missed while the server was down still starts its windows when the
  server is back, for what is left of them. One whose windows would already
  have ended is recorded as missed.
- **Run now** puts the devices in maintenance from now for the length, as
  the person who chose it.
- It doesn't run for newly enrolled devices.
- Like every policy it runs with its owner's access: saving one needs
  `devices.maintenance`, and if its owner loses it the policy stops and says
  why.

## Maintenance while installing

An update install can put its device in maintenance for as long as it takes:
**Put each device in maintenance while it installs** on an install policy or
an install job, which needs `devices.maintenance` too. As each device starts
its install, the server writes a window for it, set by the policy or the
person who started the job, that holds back alerts and every scheduled
automation. It ends when the install reports: at once, or for an install
that scheduled a restart, 15 minutes after that restart's time, so the
device's reboot and reconnect don't alert either. An install that never
reports ends its window when the server stops waiting for its report.
As above, the install's own failure still raises its alert.

## Permissions

| Permission | Allows |
| --- | --- |
| `devices.view` | Seeing windows: the Maintenance page, a device's windows and its maintenance on the wall. |
| `devices.maintenance` | Putting devices in maintenance, changing and ending windows, saving maintenance policies and asking an install to put its devices in maintenance. With `devices.view`. The Technician role has it. |

## Audit

Every change is audited against the device, one event for each window:

| Action | When |
| --- | --- |
| `maintenance_window.start` | A window starts now: a person, a maintenance policy's run or an install set it. |
| `maintenance_window.schedule` | A window is set to start later. |
| `maintenance_window.update` | Its end, reason or automation changed. |
| `maintenance_window.end` | Someone ended a window in force. |
| `maintenance_window.cancel` | Someone ended a window before it started. |

Their metadata carries the device's name, the window's ID, its reason, start,
end and automation, and the policy (with the job and update run for an
install's window). A policy's windows are attributed to the policy
(`policy “…”`), with no user. A maintenance policy's **Run now** is also
audited as `automation_policy.run` with how many windows it set. A window
reaching its end, or an install's window ending with its report, isn't an
action and isn't audited.

## API

| Request | What it does |
| --- | --- |
| `GET /v1/maintenance-windows?state=&device=&company=&policy=&limit=` | Windows: `open` (in force and upcoming, by start; the default), `current`, `upcoming`, `past` (latest first) or `all`. `company` is a company's ID or `none`. `{windows, truncated}`, at most `limit` (200 by default, up to 1,000). |
| `GET /v1/maintenance-windows/{id}` | One window. |
| `GET /v1/agents/{id}/maintenance-windows` | `{open, past}`: the device's open windows and its last 10 past ones. |
| `POST /v1/maintenance-windows` | `{target, device_ids, company_ids, tag_ids, tag_match, reason, starts_at?, ends_at? \| duration_minutes?, automation?}`: a window on each device the target (`devices`, `companies`, `tags` or `all`) covers. Without an end or a duration it lasts until ended. `201` with `{created, windows}` (the first 100). `400 invalid_maintenance` for a bad reason or time, `400 no_devices` for a target with none. |
| `PATCH /v1/maintenance-windows/{id}` | `{ends_at?, reason?, automation?}`: changes an open window; `ends_at: null` makes it last until ended. `409 window_over` for one that is over. |
| `POST /v1/maintenance-windows/{id}/end` | Ends a window in force now, or cancels an upcoming one. `409 window_over` for one that is over. |
| `POST /v1/maintenance-windows/end` | `{target, …, upcoming?}`: ends the windows in force on the devices a target covers, and with `upcoming` cancels those to come. `{ended}`. |

Reading needs `devices.view`; everything else `devices.maintenance` too. A
device's presence on the event socket (`GET /v1/agents` and `agent_upsert`)
carries `maintenance: {current, next_starts_at}` while it has a window in
force or to come: `current` names the longest-lasting window in force
(`window_id`, `reason`, `created_by`), when the earliest started, when the
device leaves maintenance as its windows stand (`ends_at`, null while one has
no end), the strictest `automation` and how many windows are in force.

Policies take `action: "maintenance"` with `maintenance_minutes`,
`maintenance_reason` and `maintenance_automation`, and install policies
`maintenance_during_install`; their list carries `last_maintenance: {devices,
ends_at}`. An install job takes `updates.maintenance: true`. Run now on a
maintenance policy answers `201 {maintenance: {devices, ends_at}}`.

## How it works

Windows are rows of `maintenance_windows` (`server/src/maintenance_windows/`),
one a device, removed with their device and purged 90 days after they end.
The server keeps the windows not yet over in memory, read again for a device
after each change, so the alert engine, the dispatcher and presence ask
nothing of the database to know whether a device is in maintenance. A task
wakes at the next start or end (and at least every 15 seconds): it ends
finished installs' windows, republishes the devices that entered or left
maintenance, re-judges the stored monitor results of those that left, and
has the alert engine evaluate at once.

Jobs carry whether they are a policy's scheduled run (`jobs.scheduled`, set
by a schedule or an enrollment), which is what maintenance skips, at
planning and again when the dispatcher would start a device.

Tests are in `server/tests/maintenance_windows.rs`: every kind of alert held
back and judged again, scheduled runs skipped and recorded and manual runs
not, start and end timing, companies, tags and every device, maintenance
policies on time, late and missed, install windows, permissions, the audit
log and the purge. `dashboard/tests/maintenance.test.mjs` covers the
website's side.
