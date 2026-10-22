# Patch management

Agents scan their computers for pending operating system updates and install
them when asked. The website shows each device's pending updates, last scan
and update history, gives the fleet an overview, installs on many devices at
once as a [job](automation.md), and installs on a schedule with
[automation policies](automation.md#policies). [Alert rules](alerts.md) watch
for updates pending too long, failed installs and restarts left pending.

## Scanning

The coordinator scans as SYSTEM or root, on its own thread, one scan or
install at a time, so the control connection never waits for a package
manager. A scan reports every pending update with:

| Field | Meaning |
|---|---|
| ID | What an install names it by: Windows Update's update ID, a macOS label, a package name (`name.arch` for DNF). |
| Title, KB | The update's title; a Windows update's Knowledge Base article. |
| Version, current version | The version it installs and the one installed now, for packages and macOS updates. |
| Category | Security, Critical, Definition, Driver, Feature pack, Service pack, Update rollup, Upgrade, Tool or Other ([below](#categories)). |
| Severity | Critical, Important, Moderate or Low, where Microsoft or the distribution rates it. |
| Size | The download size, where the platform says. |
| Restart | Installing it may need a restart. |
| Released, source | When it was published, and its repository, pocket (such as `noble-security`) or advisory (such as `ALSA-2025:1234`), where the platform says. |

A scan also says whether the computer has to restart to finish installing
updates. The server keeps the latest scan's list and remembers when each
update was first seen, which alert rules use. A failed scan (no network, a
package manager another program holds) keeps the last list and shows the
error.

Agents scan when the server asks:

- when the Agent connects and its last scan is older than the scan interval,
  or a restart was pending (so the website sees it done);
- every scan interval while it stays connected (**Settings → General**, 24
  hours by default, 0 to scan only when asked). The server asks at most 50
  devices a minute, so a fleet's package mirrors aren't hit at once;
- when a user chooses **Scan now**, a scan job runs, or a scan policy fires.

A scan the Agent hasn't reported 45 minutes after it was asked for is lost.

### Per platform

| | Windows | macOS | Linux |
|---|---|---|---|
| Service | Windows Update Agent (`IUpdateSearcher`, `IsInstalled=0 and IsHidden=0`) | `softwareupdate --list` | APT, DNF (4 and 5), YUM, pacman |
| Categories | Windows Update's classification | By title ([below](#categories)) | Security from the security pocket or advisory, else Other; Other on Arch |
| Severity | MSRC severity | No | DNF/YUM advisories |
| Size | No | Yes | APT |
| Release date | Yes | No | DNF 5 advisories |
| May need a restart | Each update's install behavior | `Action: restart` | Kernels, firmware, microcode, glibc, systemd and D-Bus |
| Restart pending | Windows Update's flag, `RebootRequired` and `RebootPending` | No | `/run/reboot-required` (APT); `needs-restarting -r`, else the newest kernel against the running one (DNF/YUM); the running kernel's modules removed (pacman) |

- **Windows** reports no size: `MaxDownloadSize` is the largest of every
  package variant (tens of gigabytes for a cumulative update), not what the
  computer downloads.
- **macOS** has no reliable "restart pending" flag, so it is never set; each
  update's own restart flag still shows. `softwareupdate` can take a minute
  or more and needs Apple's servers.
- **APT** refreshes its lists (`apt-get update`) and simulates
  `dist-upgrade`, so it lists upgrades only (new dependencies install with
  them), and leaves out phased Ubuntu updates as `apt` does.
- **DNF/YUM** runs `check-update --refresh` and reads advisories from
  `updateinfo` (DNF 4) or `advisory list` (DNF 5).
- **pacman** uses `checkupdates` when installed, else syncs into a temporary
  database, never the real one: a sync without an upgrade would leave Arch
  partly upgraded.
- **zypper** (openSUSE, SLES) is not implemented. Such devices, and any
  other without a supported manager, show "no supported update manager"; jobs
  skip them.

### Categories

Windows Update classifies every update. Linux and macOS updates are put in
the categories that matter for choosing what to install:

- **Linux:** an update from a security pocket (`*-security`,
  `Debian-Security`) or with a security advisory is Security; every other is
  Other. Arch Linux publishes no advisories, so all its updates are Other.
- **macOS:** a newer major macOS is Upgrade; other macOS updates, Background
  Security Improvements and Rapid Security Responses are Security; XProtect,
  MRT, Gatekeeper and configuration data are Definition; everything else
  (Safari, Command Line Tools) is Other.

## Installing

An install chooses chosen updates, the updates in chosen categories, or
every pending update, and what happens afterwards:

- **Restart:** never; if required (an installed update, or the computer,
  says so); or always.
- **Delay:** 1 minute to 24 hours (5 by default) of warning before the
  restart.

The Agent scans first and installs only what is pending then (chosen updates
no longer pending fail with "It is no longer pending"), reports progress such
as "Installing 2 of 5: …", installs, scans again, and sets the restart. Each
update's outcome is kept: Installed or Failed with the error. The install as
a whole **succeeded** (every chosen update installed, or none was pending),
was **partial**, or **failed** (nothing installed, or it couldn't start, such
as when another program holds the package manager); one the Agent hasn't
reported five hours after it started is **lost**. A device installs one
thing at a time: a second install while one runs fails at once, and on the
device page **Install** is refused (`409 install_running`).

| | Windows | macOS | Linux |
|---|---|---|---|
| How | Download in one batch, then install each update on its own (`IUpdateInstaller`, quiet) | `softwareupdate --install <label>` per update, `--all` for everything | APT: `apt-get install --only-upgrade`, `dist-upgrade` for everything; DNF/YUM: `upgrade <name.arch>`; pacman: `pacman -Syu` |
| Chosen updates | Yes | Yes | Yes; pacman only everything |
| Per-update outcome | The installer's result for each | Whether its label is still pending | Whether the new version is installed |
| Progress | Per update | Per update | Per install |
| Restart notice | Windows' countdown notification with the message | `shutdown` warns users at a terminal | `shutdown` warns users at a terminal (`wall`) |

- **Windows:** once an update says the computer must restart before it can
  install more, the remaining ones fail with that reason. Licence terms are
  accepted.
- **macOS:** on Apple silicon, `softwareupdate` needs a volume owner's
  password to install macOS itself as root, which the Agent doesn't have:
  such an install fails with "On Apple silicon, macOS updates must be
  installed by a user at the Mac". Other updates (Safari, XProtect, Command
  Line Tools) install.
- **Linux:** APT runs non-interactively, keeps changed configuration files,
  waits up to 10 minutes for the dpkg lock, and suspends `needrestart` so it
  can't restart the Agent mid-install. Arch Linux supports only upgrading
  everything at once, so pacman devices offer **Install all** only (`400
  selection_unsupported` otherwise).
- **Restarts** use the platform's scheduled shutdown (`InitiateShutdownW`
  with a grace period on Windows, `shutdown -r +N` on macOS and Linux), so
  the countdown survives the Agent restarting. The message tells the user
  their IT administrator installed updates and to save their work. Desktop
  users on macOS and Linux see no window; that would need a notice in the
  session helpers, which isn't implemented. After the restart the Agent
  reconnects and scans, and the website shows the restart done.

## The website

- **A device's Updates tab** (`/device?id=…&tab=updates`): the update
  manager, last scan, a scan error, a pending restart, the scan or install in
  progress with its progress, **Scan now**, the pending updates by category
  with checkboxes, **Install selected** and **Install all** with the restart
  choice, and the install history with each update's outcome, who or which
  policy started it, and the restart.
- **Updates** in the navigation (`/updates`): every device with its pending,
  security and critical counts, the oldest pending update's age, last scan,
  restart pending and last install; tiles and filters for devices with
  security updates pending, failed installs, restarts pending and devices not
  scanned for three days, by company, [tag](devices.md#tags) and operating
  system; and, for chosen
  devices, **Scan now** and **Install updates…** as a job.
- **Policies** can scan for updates or install them (chosen categories, or
  all, with the restart choice), on any schedule and target policies have,
  tags included: an install policy on a tag such as "Pilot ring" covers the
  devices tagged when it runs.
- Both pages update live from the event socket.

Bulk installs and scans are ordinary [jobs](automation.md): each device's
part is an update run, Succeeded when every chosen update installed, Failed
when some failed, No result when the device never reported, and Skipped when
the device has no supported update manager. A policy's channels get
`job.failed` when its install job has failures.

## Alerts

| Condition | Raised when | Resolves |
|---|---|---|
| Updates pending | Updates of a category (or any) have been pending for N days (1 to 365), by when a scan first listed them | When none is pending that long |
| Update install failed | An install failed, partly or entirely, or never reported | By a person |
| Restart pending | The device has had to restart for N days | When a scan says it no longer has to |

## Permissions and audit

| Permission | Allows | Technician |
|---|---|---|
| `updates.view` | Seeing pending updates, scans, history and the fleet page; **Scan now**; scan jobs. | Yes |
| `updates.install` | Installing updates and restarting for them; install jobs. | Yes |
| `jobs.run_wide` | Jobs on whole companies, tags or every device. | No |
| `automation.manage` | Policies; saving an update policy also needs `updates.view` (scan) or `updates.install` (install), which it then runs with. | No |

The audit log records `update.scan` and `update.install` (with the
selection and restart) against the device, `job.start` for jobs, and
`automation_policy.*` for policies.

## Retention

Installs, the update history, are kept for a year; scans the server asked
for, like jobs, for 30 days. A deleted device's status and pending list go
with it.

## API

| Route | |
|---|---|
| `GET /v1/agents/{id}/updates` | `{device_id, status, updates}`: the device's summary (below) with `scanning` (`{run_id, created_at}`) and `installing` (a run) or null, and its pending updates. |
| `POST /v1/agents/{id}/updates/scan` | 202 with the scan's run (an already pending scan's). `409 device_offline`. |
| `POST /v1/agents/{id}/updates/install` | `{update_ids, categories, reboot, reboot_delay_minutes}`; IDs win over categories, neither means everything. 202 with the run. `409 device_offline`, `409 install_running`, `400 not_scanned`, `400 unsupported`, `400 selection_unsupported`, `400 invalid_install`. |
| `GET /v1/update-runs?device_id=&kind=&limit=` | Runs, newest first (50 by default, at most 200); installs with `items`. |
| `GET /v1/update-runs/{id}` | |
| `GET /v1/updates/devices` | Every enrolled device's summary. |
| `POST /v1/jobs` | With `updates: {action: "scan"}` or `{action: "install", categories, reboot, reboot_delay_minutes}` instead of a script or file. |
| `POST/PUT /v1/automation-policies` | With `action` `update_scan` or `update_install` (`update_categories`, `reboot`, `reboot_delay_minutes`). |

A summary reads `{device_id, device_name, company_id, platform, connected,
manager, supported, can_select, scanned_at, received_at, scan_error,
reboot_pending, reboot_pending_since, pending, security, critical,
oldest_pending_at, scanning, installing, last_install}`. A run reads `{id,
device_id, device_name, kind, status, progress, progress_percent, update_ids,
categories, reboot, reboot_delay_minutes, requested_by, policy_name, job_id,
error, installed, failed, reboot_required, reboot_scheduled_at,
reboot_error, created_at, completed_at, items}`. The event socket sends
`{"type": "device_updates", "device": …}`, `{"type": "update_run", "run": …}`
(without items) and `{"type": "updates_resync"}` to users with
`updates.view`.

## How it's built

- Wire: `crates/protocol-types/src/updates.rs`. The server sends
  `scan_updates {run_id?}` and `install_updates {install}`; the Agent sends
  `update_scan {run_ids, scan}`, `update_progress` and `update_install
  {run_id, result}` on the control connection. Reports wait in a queue that
  outlives reconnects.
- Agent: `agent/src/remote/updates.rs` (one operation at a time, choosing
  what to install, the restart) with `windows.rs` (WUA COM), `macos.rs`,
  `linux.rs` and `linux/` (`apt.rs`, `rpm.rs`, `pacman.rs`, `reboot.rs`),
  and `process.rs` (tools with time limits). Restarts are
  `power::schedule_restart`.
- Server: `server/src/updates/` (`store.rs` for status and pending lists,
  `runs.rs` for runs and their items, `mod.rs` for the engine, reports and
  events) and `server/src/api/updates.rs`. Jobs and policies gained update
  kinds in `server/src/automation/`; the alert conditions are in
  `server/src/alerts/`.
- Tables: `device_update_status`, `device_pending_updates`, `update_runs`,
  `update_run_items`; `jobs` and `automation_policies` have the install's
  categories and restart; `alert_rules` has `update_category` and `days`;
  `alerts` has `update_run_id`; `settings` has `update_scan_interval_hours`.
- Website: `dashboard/features/updates/`.

## Tests

- `cargo test -p meshrmm-server --test updates`, on SQLite and PostgreSQL:
  scans, pending lists and first-seen times, scans and installs from the
  website with progress and outcomes, pacman and unsupported devices, update
  jobs and policies, the three alert conditions, lost runs and the setting.
- `cargo test -p meshrmm-agent updates` runs the parsers of every platform's
  tool output. The ignored `real_scan_is_plausible` tests scan the computer
  they run on (as root or SYSTEM); `real_install_installs_the_chosen_updates`
  installs on a Linux test machine (`MESHRMM_TEST_INSTALL=ids|all`).
- `npm test` in `dashboard` covers the updates model.
