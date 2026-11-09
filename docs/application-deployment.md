# Application deployment

MeshRMM installs, updates and uninstalls third-party applications with each
platform's package managers, or with installer files from the
[toolbox](toolbox.md). Agents scan for the applications those managers have
installed and outdated. The website shows them on each device and for the
whole fleet, and keeps an application **library** of what to deploy.
Installs, updates and uninstalls run on one device or as
[jobs](automation.md) on many. [Policies](automation.md#policies) keep
applications installed and up to date, and [alert rules](alerts.md) watch
for failed installs and updates left pending.

Operating system updates are [patch management](patch-management.md)'s, so
APT, DNF and pacman packages aren't listed here.

## Package managers

| Platform | Sources | Runs as |
|---|---|---|
| Windows | winget, Chocolatey | SYSTEM |
| macOS | Homebrew formulae and casks | The user who owns the Homebrew installation |
| Linux | Flatpak (the system installation), Snap | root |
| Any | A custom installer: a toolbox file | SYSTEM or root |

A device uses the managers it has. MeshRMM doesn't install a package manager
itself; a device without one lists no packages.

- **winget** comes with the App Installer package, which registers it only
  for signed-in users. The SYSTEM service runs the `winget.exe` in the newest
  `Microsoft.DesktopAppInstaller_*` folder under `WindowsApps`, from that
  folder. Every command gets `--accept-source-agreements` and
  `--disable-interactivity`, and installs and upgrades get `--exact`,
  `--silent` and `--accept-package-agreements` too. Installs ask for machine
  scope (`--scope machine`). A package with no machine-scope installer falls
  back to its default installer. winget's `--exact` matches case, so
  updates and uninstalls use the ID as the device's scan lists it. An
  install whose ID winget doesn't find as written is tried again with the
  ID `winget show` gives it.
- **Chocolatey** commands get `-y --no-progress -r`. Chocolatey 1 is listed
  with `list -l`, Chocolatey 2 with `list`.
- **Homebrew** refuses to run as root. The Agent finds `brew` at
  `/opt/homebrew/bin/brew` or `/usr/local/bin/brew` and runs it as the owner
  of that file:
  - It uses `sudo -n -u #<uid> -H`, which never asks for a password.
  - It runs from `/private/tmp`, with `HOMEBREW_NO_AUTO_UPDATE`,
    `HOMEBREW_NO_INSTALL_CLEANUP`, `HOMEBREW_NO_ENV_HINTS`,
    `HOMEBREW_NO_ANALYTICS` and `NONINTERACTIVE` set.
  - It refuses an installation that root owns.
  - Package names are given to it in lowercase, as Homebrew writes them.
  - A cask whose installer needs an administrator's password fails, as it
    would for that user without a terminal.
- **Flatpak** works on the system installation (`--system`), with `LC_ALL=C`.
  An update whose remote doesn't give a version shows as "a newer build".
- **Snap** is used when `/run/snapd.socket` exists. An install that the
  store says needs classic confinement is tried again with `--classic`.

## Scanning

A scan lists every package each manager has installed, with its version,
and the newer version the manager offers when it's outdated:

| Source | Commands |
|---|---|
| winget | `winget list` (the Available column gives the newer version) |
| Chocolatey | `choco list -r` and `choco outdated -r` |
| Homebrew | `brew update --quiet` (a failure is ignored), `brew list --formula --versions`, `brew list --cask --versions` and `brew outdated --json=v2` |
| Flatpak | `flatpak list --system --app` and `flatpak remote-ls --system --updates --app` |
| Snap | `snap list` and `snap refresh --list` |

Agents scan:

- with every [update scan](patch-management.md#scanning), so on the same
  schedule (**Settings → General**, 24 hours by default);
- when a user chooses **Scan now** on a device's **Applications** tab;
- before and after every install, update and uninstall.

Each manager's scan has 15 minutes for all its commands together, and the
whole scan 30 minutes; a manager still scanning then fails. A manager whose
scan fails keeps the packages from its last scan, and the device's tab shows
the error. The server keeps each device's latest list and remembers when
each package was first found outdated, which the fleet view and alert rules
use. A scan the Agent hasn't reported 45 minutes after it was asked for is
lost.

winget's table is read by the positions of the header's labels, which are
in the device's language. Output that can't be read as a table fails the
scan rather than passing for an empty list. A package whose installed
version winget lists as Unknown isn't outdated, as plain `winget upgrade`
sees it: **Update all** leaves it alone, it doesn't count toward
[Application updates pending](#alerts), and updating it must be asked for by
name.

## The library

**Applications → Library** lists the applications to deploy. Each one has:

- a name, unique ignoring case, and a description;
- one to 12 **ways to install**, each one of:
  - a winget, Chocolatey, Homebrew formula, Homebrew cask, Flatpak or Snap
    package ID, for that manager's platform;
  - a **custom installer**, made of:
    - a shared toolbox file and the platform it is for;
    - its silent install arguments;
    - an optional uninstall command;
    - a **detection rule** (below).
- an optional **pinned version**:
  - winget and Chocolatey install exactly that version;
  - updates leave the application alone everywhere: it shows as **Pinned**,
    isn't chosen by **Update all**, and doesn't count toward
    [Application updates pending](#alerts).

A device installs an application with the first way listed for its platform
that it can use: a custom installer, or a package whose manager the device
has. The library also shows how many devices have each application installed
and how many have it outdated. A device's package belongs to a library
application when it matches one of the application's package sources.

Package IDs match as their manager reads them. winget, Chocolatey and
Homebrew ignore case, so the library's `mozilla.firefox` is a device's
`Mozilla.Firefox` for counts, pins, updates, the fleet view and alerts.
Flatpak IDs match case exactly, and snap names are lowercase.

### Custom installers

| Platform | Files | Run as |
|---|---|---|
| Windows | `.msi` through `msiexec /i` (`/qn /norestart` when no arguments are given), `.exe` with the arguments, `.ps1` through Windows PowerShell, `.cmd` and `.bat` through `cmd.exe` | SYSTEM |
| macOS | `.pkg` through `installer -pkg … -target /`, `.sh` through `/bin/sh` | root |
| Linux | `.deb` through `apt-get` (or `dpkg`), `.rpm` through `dnf`, `yum`, `zypper` or `rpm`, and `.sh`, `.run` and `.bin`: a program (ELF) runs itself, a script runs through `/bin/sh` | root |

A detection rule tells whether the application is installed:

| Rule | Installed when |
|---|---|
| Installed application | The device's [software list](hardware-and-software.md) has an application of that name (`*` matches any text), at least the minimum version if one is given. |
| File or folder | The path exists. Windows paths may use `%ProgramFiles%` and other environment variables. |
| Registry key (Windows) | The key exists in `HKLM`, `HKCU`, `HKU` or `HKCR`, in either registry view, with the named value if one is given. |

An install that the rule already finds does nothing. An installer that exits
with 0 (3010 and 1641 mean done, needing a restart) while the rule still
doesn't find the application fails.

The uninstall command runs in `cmd.exe` on Windows and `/bin/sh` elsewhere.
An uninstall that the rule still finds afterwards fails. Without an uninstall
command, the library can't uninstall that way of installing. On Windows, the
program can still be uninstalled from the device's software list (below).

Adding a custom installer to the library, or changing one (its file,
platform, arguments, uninstall command or detection rule), needs
`scripts.run` as well as `applications.manage` (`403 permission_denied`
otherwise), for a user and for a limited API token alike. Its commands run
as SYSTEM or root on every device a policy installs it on. Without
`scripts.run`, the editor shows custom installers but can't change them;
package manager sources, the rest of the application, and removing a custom
installer stay `applications.manage`'s.

On Windows, an installer or uninstall command that exits with 1618 (another
installation is in progress) is run again up to three times, half a minute
apart.

The Agent downloads the installer from the server for that run only. It saves
it in a folder only its own account can use, checks its size and SHA-256 on
the way, and removes it afterwards. The server serves a run's installers only
while the run is pending, and only the files the run's install names. A
toolbox file that the library uses can't be deleted or unshared
(`409 file_in_use`).

## Installing, updating and uninstalling

On a device's **Applications** tab:

- **Install from the library…** chooses library applications for the
  device's platform.
- **Update all** updates every outdated package except pinned ones, up to
  50 at a time; the rest wait for the next update.
  **Update** on a package, or **Update N selected**, updates only those.
- **Uninstall** on a package, or **Uninstall N selected**, uninstalls with
  the package's manager.
- **Uninstall a program…** (Windows) chooses programs from the device's
  software list that are installed for every user. The Agent runs the
  program's quiet uninstaller, or `msiexec /x {product code} /qn /norestart`
  for a Windows Installer package. It waits up to two minutes for the
  program to leave the list, since some uninstallers hand the work to a copy
  of themselves and exit at once. A program with neither fails, and
  per-user programs aren't offered.

On the **Applications** page, **Install on devices** (a library application)
and the **Outdated** tab's **Update on their devices…** and **Update all…**
start a job on chosen devices, companies, tags or every device, which waits
for offline devices or skips them. The job's page follows every device, and
**View applications** opens a device's result.

### How a run works

A device does one scan or run at a time, on its own thread, so the control
connection never waits for a package manager. The server refuses a second
run while one is in progress (`409 application_run_running`), and the
database holds at most one pending run and one pending scan per device, so
two requests at once can't both start one. A job holds a device's part back
until the device is free.

1. The Agent scans, so it knows what is installed and outdated.
2. It works through the applications one at a time, reporting progress. Each
   has 30 minutes, and all of them four hours: one started near the end gets
   only what is left, and whatever is left after that fails. A program that
   runs too long is stopped with what it started: a process group on macOS
   and Linux, and on Windows a job object the program is put in before it
   runs. Windows Installer does its work in its own service (`msiserver`),
   outside the job, so an `.msi` install can go on after its time is up.
3. It scans again, so the website shows the result.

Each application ends as one of:

| Outcome | Meaning |
|---|---|
| Done | Installed, updated or uninstalled. |
| Nothing to do | Already installed, already up to date, or not installed. |
| Failed | The manager or installer failed, timed out, or isn't on the device; the error and the end of the output say why. |

The run succeeds when nothing failed, partly fails when some did, and fails
when all did or it couldn't start. A run that needs a restart says so. MeshRMM
doesn't restart for applications; use [patch management](patch-management.md)
or a script for that. A result that finishes while the device is offline is
reported when it reconnects. A run that hasn't reported five hours and 15
minutes after it started is lost.

Each application keeps the last 16,384 characters of its output, and a run
1 MiB of output in all, shared evenly between its applications, so a result
always fits a message to the server.

Every time the Agent connects, it first says which scans and runs it is
doing or has yet to report. Any other the server asked for more than 30
seconds before that connection is lost at once ("The device's Agent
restarted, or the device did, before it finished"): the Agent restarted,
the device rebooted, or the connection dropped before the Agent got it. (One
asked for within those 30 seconds may be on its way on the new connection,
so it waits for its report window.) The device is free for
the next run, and **Scan now** asks for a new scan.

## Policies

Two policy actions, under **Jobs → Policies**:

- **Install applications** installs chosen library applications. It runs on
  its schedule, and also on each newly enrolled device it covers, whatever
  its trigger, so new devices get the standard applications. Devices that
  have them already are left as they are. Devices on a platform none of the
  applications is for are skipped.
- **Update applications** updates every outdated application, only chosen
  ones, or every one except chosen ones. The chosen ones are library
  applications, and other packages written as `source:id` (such as
  `winget:Mozilla.Firefox`). Pinned applications are never updated.

A policy reads the library each time it runs, so changing an application
changes what its policies do next. A library application that a policy names
can't be deleted (`409 application_in_use`). Saving one of these policies
needs `automation.manage` and `applications.deploy`.

## Alerts

| Condition | Raised when | Resolved |
|---|---|---|
| Application install failed | An install or update of applications, by hand, in a job or by a policy, failed partly or entirely, or never reported back. Each failed application gets its own alert; a lost run gets one. | Never: a person resolves it. |
| Application updates pending | The device's last scan lists outdated applications, other than pinned ones, that have been outdated for more than N days. | When none has been outdated that long. |

## Permissions and audit

| Permission | Allows | Technician |
|---|---|---|
| `applications.view` | Seeing the library, devices' packages, the fleet's outdated ones and the application history, and asking devices to scan. | Yes |
| `applications.deploy` | Installing, updating and uninstalling on devices, one at a time or as jobs (whole companies, tags or every device need `jobs.run_wide`), and saving application policies. | Yes |
| `applications.manage` | Adding, changing and deleting library applications. Adding or changing a custom installer, which runs as SYSTEM or root with the arguments and commands as written, needs `scripts.run` too. | No |

Personal [API tokens](api.md#api-tokens) work with every route. The audit
log records:

- `application.create`, `application.edit` and `application.delete`, with
  the application's sources;
- `application.scan`, `application.install`, `application.update` and
  `application.uninstall` for each run on one device, with what it covers;
- jobs as `job.start`, whose metadata includes `applications`.

## Security

- Package IDs reach a manager's command line as their own arguments, never
  through a shell. They are limited to the characters each manager uses,
  start with a letter or digit, never contain `..`, and can't be read as an
  option. Versions are limited the same way. The server checks them when an
  application, request or policy is saved, and the Agent checks them again.
- A custom installer's arguments and uninstall command are passed as
  written: they are commands that run as SYSTEM or root. That is why
  changing the library needs `applications.manage`, which the Technician
  role doesn't have, and adding or changing a custom installer `scripts.run`
  too, the permission to run scripts as SYSTEM or root. They must fit on one
  line of at most 2,000 characters.
- The Agent fetches installers with its own credentials, only for a pending
  run that names them, and checks their SHA-256 against the toolbox's.

## API

| Route | |
|---|---|
| `GET /v1/applications` | The library: each application's `sources`, `pinned_version`, `installed` and `outdated` device counts, and the `policies` that use it. |
| `POST /v1/applications`, `PUT /v1/applications/{id}` | `{name, description, pinned_version, sources: [{source, package_id}` or `{source: "custom", platform, file_id, install_arguments, uninstall_command, detection}]}`, where `detection` is `{kind: "application", name, min_version}`, `{kind: "file", path}` or `{kind: "registry", key, value}`. `409 application_name_taken`; `403 permission_denied` for adding or changing a custom installer without `scripts.run`. |
| `DELETE /v1/applications/{id}` | `409 application_in_use` while a policy names it. |
| `GET /v1/agents/{id}/packages` | `{device_id, status: {managers, scanned_at, received_at}, scanning, running, packages}`: each package's `source`, `package_id`, `name`, `version`, `available_version`, `outdated_since`, `application_id`, `application_name` and `held`. |
| `POST /v1/agents/{id}/packages/scan` | Asks the online device to scan; returns the scan's run, or the scan already asked for. |
| `POST /v1/agents/{id}/application-runs` | `{action: "install", application_ids}`, `{action: "update", mode: "all"}` or `{action: "update", mode: "only" or "except", application_ids, packages}`, or `{action: "uninstall", application_ids, packages, inventory: [{name, version}]}`, with packages as `source:id`. Returns the run. `409 device_offline`, `409 application_run_running`, `400 not_applicable`. |
| `GET /v1/application-runs?device_id=&action=&limit=` | Runs, newest first (50 by default, at most 200). |
| `GET /v1/application-runs/{id}` | A run with each application's `outcome`, `version`, `exit_code`, `error` and `output`. |
| `GET /v1/packages/outdated` | Every outdated package in the fleet, with the devices it's outdated on, most devices first. |
| `POST /v1/jobs` | `{applications: {…as for a run…}}` with the [job's target](automation.md#api). |

A run reads `{id, device_id, device_name, action, summary, status, progress,
requested_by, policy_name, job_id, error, succeeded, failed, reboot_required,
created_at, completed_at, items}`, where `action` is `scan`, `install`,
`update` or `uninstall`, and `status` is `pending`, `succeeded`, `partial`,
`failed` or `lost`.

```sh
# Install Firefox (a library application) on one device.
curl -sH "Authorization: Bearer $MESHRMM_TOKEN" -H "Content-Type: application/json" \
  -d '{"action": "install", "application_ids": ["'"$FIREFOX"'"]}' \
  "$MESHRMM/v1/agents/$DEVICE/application-runs"

# Update everything outdated on every device in a company, waiting a day for offline ones.
curl -sH "Authorization: Bearer $MESHRMM_TOKEN" -H "Content-Type: application/json" \
  -d '{"applications": {"action": "update", "mode": "all"}, "target": "companies",
       "device_ids": [], "company_ids": ["'"$COMPANY"'"], "tag_ids": [], "offline": "wait", "wait_hours": 24}' \
  "$MESHRMM/v1/jobs"
```

## Retention

Runs and their applications' results are kept for a year, as each device's
application history. A deleted device's packages go with it; its runs stay.

## Limits

- Per-user programs on Windows (installed for one user only) aren't offered
  for uninstalling; neither are programs with no quiet uninstaller that
  aren't Windows Installer packages.
- A pinned version installs with winget and Chocolatey only. Homebrew,
  Flatpak and Snap install their current version, though pinned applications
  are still held back from updates everywhere.
- An existing device first lists its applications at its next update scan,
  or on **Scan now**. With the update scan interval at 0, devices list
  applications only when asked or around a run.
- Alerts name the failed application, but don't link to its run; the
  device's **Applications** tab has the history.

## How it's built

- `crates/protocol-types/src/applications.rs`: the wire types, the package ID
  and version rules, and the limits and timeouts.
- `agent/src/remote/applications.rs`: scans and runs on their own thread, one
  at a time, and the results queue that outlives reconnects. The files in
  `agent/src/remote/applications/` are:
  - `windows.rs`, `macos.rs` and `linux.rs`: the managers and installers;
  - `parse.rs`: the managers' output, tested on captured output in
    `fixtures/`;
  - `download.rs`: installer downloads;
  - `process.rs`: running with timeouts and the scan's or run's deadline.
- `server/src/applications/`:
  - `library.rs`: the library and its checks;
  - `store.rs`: devices' packages and the fleet's outdated ones;
  - `runs.rs`: runs and their lost and history sweeps;
  - `jobs.rs`: what a run or job does on each platform;
  - `policies.rs`: application policies;
  - `mod.rs`: the engine's task and the Agent's reports.
- The API is `server/src/api/applications.rs`. The alert rules are
  `server/src/alerts/applications.rs`.
- Tables: `applications` and `application_sources` (the library),
  `device_application_status` and `device_packages` (each device's last
  scan), `application_runs` and `application_run_items` (the history),
  `application_jobs` and `application_policies` (what a job or policy does).
- The website is `dashboard/features/applications/`.

## Tests

- `cargo test -p meshrmm-agent applications` parses winget, Chocolatey,
  Homebrew, Flatpak and Snap output. `-- --ignored` scans the computer it
  runs on, read-only.
- `cargo test -p meshrmm-protocol-types` covers package ID and version
  rules, version comparison and name matching.
- `cargo test -p meshrmm-server --test applications`, on SQLite and
  PostgreSQL, covers:
  - the library's checks and permissions, including `scripts.run` for
    custom installers;
  - package IDs that differ in case;
  - two requests at once, and runs a restarted Agent no longer has;
  - scans, installs, updates and uninstalls on a device;
  - jobs and policies, including enrollment;
  - alerts and lost runs;
  - API tokens.
- `npm test` in `dashboard` covers the library editor (including which
  changes touch a custom installer), the policy and alert models, and
  which pages show.
