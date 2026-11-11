# Remote tools

A device's processes and services in the website, without a remote session:
see what runs, end a process or a whole tree of them, and start, stop,
restart or change how a service starts. Like the [file browser](files.md)
and the [terminal](terminal.md), they reach headless servers as easily as
desktops.

## Opening them

**Processes** and **Services** on an online device's page, beside
**Terminal** and **Files**, open `/tools?id=…&tool=…` in a new browser tab,
with a tab for each tool the user may see. A tool the device's platform
lacks shows as unavailable, with why, instead of disappearing.

Every tool runs as the Agent's own account, **SYSTEM** on Windows and
**root** on macOS and Linux. There is no signed-in-user mode: a user's view
of the process list or the service manager would hide most of what a
technician comes for.

## Processes

The list shows each process's name, process ID, user, CPU, memory and start
time, its command line under its name, and on Windows its session (0 for
services, 1 and up for signed-in users). Search narrows it by name, ID,
user, path or command line; every column sorts, either way. **Tree** shows
each process under the one that started it, with each level sorted and
collapsible; a search keeps the ancestors of each match, so it stays in its
place. A process whose parent ID now belongs to a process started after it
(the system reuses IDs) is a root of its own.

- **CPU** is the process's share of the whole computer's processor time
  since the Agent's previous reading, 0 to 100%, as Task Manager shows it:
  a process keeping one of eight cores busy shows 12.5%. A first reading,
  or one after a minute without any, takes two samples half a second apart.
- **Memory** is the working set on Windows and resident memory on macOS and
  Linux.

While the page is open and its tab is shown, it reads the list again every
3 seconds. A hidden tab, or **Pause**, asks the device for nothing; showing
the tab again, or **Resume**, reads it at once and goes on. The status line
says which, and when the list was read.

### Ending processes

With `tools.processes.manage`, each row has **End** and **End tree**, and
checkboxes choose several to end together. A confirmation lists every
process that will end, by name, ID and user; **End tree** lists the process
and everything it started, from the list on screen. The device then ends
exactly those, and only while each is still the process listed: the server
sends each one's start time, and one whose ID now names another process
fails as `changed` and is left alone. Children end before their parents.

- **Windows**: `TerminateProcess`, after checking the creation time on the
  same handle it ends. A process Windows marks as critical is refused.
- **Linux**: `SIGTERM`, then `SIGKILL` five seconds later if it still runs,
  each through a pidfd (Linux 5.3 and newer) opened before the start time
  is checked, so the ID can't pass to another process in between.
- **macOS**: the same signals, after checking the start time with libproc.

The Agent never ends itself or its own processes, the first process, the
kernel's threads, or what the system can't do without (the same list
[self-healing](monitors.md#self-healing) keeps to, such as `csrss`,
`lsass`, `svchost`, `launchd` and `WindowServer`). Those fail as
`protected`. The answer lists what ended and why the rest didn't.

## Services

The list shows each service's display name and name, description, state,
how it starts, the account it runs as and its main process ID, and the
Agent's own service marked as such. It's read when the tab opens and with
**Refresh**; **All**, **Running** and **Not running** filter it, search
narrows it, and the name, state, startup and account columns sort.

| | Windows | macOS | Linux |
|---|---|---|---|
| What's listed | Win32 services in the Service Control Manager | Jobs in launchd's system domain (`launchctl list`), and job files in `/Library/LaunchDaemons` and `/System/Library/LaunchDaemons` not loaded | systemd service units, loaded or with a unit file; templates (`name@.service`) aren't |
| Name, display name | Key name, display name | Label | Unit |
| Description | The service's | The job file's path | The unit's `Description=` |
| Account | The logon account | The running job's process's user | `User=`, root when unset |
| Start | `StartService`, waiting until it runs | `launchctl bootstrap` when not loaded, then `kickstart` | `systemctl start` |
| Stop | `ControlService` stop, waiting until it stops | `launchctl bootout` | `systemctl stop` |
| Restart | Stop, then start | `launchctl kickstart -k` | `systemctl restart` |
| Startup types | Automatic, Automatic (Delayed Start), Manual, Disabled | Enabled, Disabled (`launchctl enable`, `disable`) | Enabled at boot (`enable`), on demand (`disable`), masked (`mask`); static units can't be changed |

Start, stop and restart ask first, and wait up to two minutes for the
service to get there. A Windows service that stops as it starts fails with
its exit code; one that has dependents running fails as `dependents`,
naming them as Windows does. **Startup…** sets how a service starts from
then on without starting or stopping it.

The Agent's own service can't be stopped, restarted or changed: that would
cut the device off. Those fail as `protected`.

## Permissions

| Permission | Allows | Technician |
|---|---|---|
| `tools.processes.view` | See processes, with their users and command lines | Yes |
| `tools.processes.manage` | End processes and process trees | Yes |
| `tools.services.view` | See services | Yes |
| `tools.services.manage` | Start, stop and restart services and change how they start | Yes |

A change permission doesn't include its view; a role usually holds both.
Each route checks its device in one place (`authorized_device` in
`server/src/api/tools.rs`), which a device the user may not reach answers
as not found. API tokens may use every route, within their permissions and
their user's.

## Audit

Lists aren't recorded. Every change the device was asked for is, on the
device:

- `process.end`: the `processes` that ended (`pid`, `process` name), their
  `count`, the `failed` ones with their `code`, whether it was a `tree`, and
  `result`: `completed`, `partial` or `failed`.
- `service.start`, `service.stop`, `service.restart`: the `service`, its
  `display_name` and `state` afterwards, and `result`.
- `service.set_startup`: the `service`, the `startup` asked for, the
  `startup_now` the device reports, and `result`.

A change the device refused is recorded with `result` `failed` and the
`error` code. One it never confirmed, because it didn't answer in time or
disconnected, is recorded with what was asked (for `process.end`, the
`pids`) and `result` `unconfirmed`, with `error` `device_timeout` or
`device_disconnected`: the device may have made it. One that never reached
the device, because it was offline or busy, isn't recorded.

## Errors

The Agent's refusals reach the website with a code and a sentence:
`not_found`, `access_denied`, `protected`, `changed`, `invalid`,
`dependents`, `unsupported`, `busy`, `timed_out` and `failed`. A device
that is offline is `device_offline` (409); one with as many requests
waiting as it may, or whose connection's backlog is full, `device_busy`
(503, with `Retry-After`); one that disconnects while it works,
`device_disconnected`; one that doesn't answer in time, `device_timeout`
(504).

## How it works

```text
browser ── HTTPS /v1/agents/{id}/processes… ──▶ server ── control connection: tool_request ──▶ Agent
        ◀────────────── JSON ────────────────        ◀────────── tool_reply ─────────────
```

As with the file browser's listings, each request is small and goes over
the Agent's control connection: the server sends `tool_request`
(`crates/protocol-types/src/tools.rs`) and waits for the `tool_reply` with
the same `request_id`.

- **The server's wait.** 30 seconds for a list, two minutes for a change,
  sent with the request as `timeout_ms`. The Agent counts it from the
  request's arrival on its monotonic clock and answers 5 seconds before it
  ends (a quarter of it, for a shorter wait). A device has at most 16
  requests waiting (`MAX_PENDING_PER_DEVICE`); a request the browser
  abandons is forgotten at once.
- **On the Agent.** Four requests run at once, each on a blocking thread;
  one whose time runs out while it waits its turn answers `timed_out`
  without starting. A service manager that never answers can't be stopped:
  five seconds after the server stopped waiting, the request gives up its
  turn, and while eight are left behind new ones are refused as `busy`.
- **Size.** A list is sent whole, so the website sorts, filters and draws
  its tree itself, but bounded: at most 5,000 processes, each command line
  cut to 2 KB and all command lines and paths together to 1 MB; at most
  5,000 services with descriptions cut to 512 bytes. Even the largest list
  stays well within the control connection's 4 MiB messages. The page
  draws at most 2,000 rows; search narrows the rest.

| Route | Permission | |
|---|---|---|
| `GET /v1/agents/{id}/processes` | `tools.processes.view` | `{processes, read_at, logical_cpus, memory_total_bytes, truncated}` |
| `POST /v1/agents/{id}/processes/end {processes: [{pid, started_at}], tree}` | `tools.processes.manage` | 1 to 256 processes; `{ended, failed}` |
| `GET /v1/agents/{id}/services` | `tools.services.view` | `{services, read_at, truncated}` |
| `POST /v1/agents/{id}/services/control {name, action}` | `tools.services.manage` | `action` `start`, `stop` or `restart`; the service afterwards |
| `POST /v1/agents/{id}/services/startup {name, startup}` | `tools.services.manage` | `startup` `automatic`, `automatic_delayed` (Windows), `manual` or `disabled`; the service afterwards |

A service name must be one the device's platform has: a Windows key name
without slashes, a systemd unit name, or a launchd label.

Server: `server/src/tools/`, `server/src/api/tools.rs`. Agent:
`agent/src/remote/tools.rs` (the requests, CPU readings and the order to end
processes in) and `tools/` (`windows`, `linux`, `macos`, `unix` for
signals, and `parse` for the system tools' output). Website:
`dashboard/features/tools/`.

## Tests

- `cargo test -p meshrmm-protocol-types tools` covers the wire types and
  the size bounds.
- `cargo test -p meshrmm-server --test tools` runs on SQLite and
  PostgreSQL: lists and changes through a stand-in Agent, who may do what,
  API tokens, the audit log with failed and unconfirmed changes, offline and
  busy devices, the per-device bound and bad requests.
- `cargo test -p meshrmm-agent tools::` reads every platform's tool output,
  lists this computer's processes, ends a child process the test starts and
  refuses the Agent and a process whose start time doesn't match, and runs
  the request scheduler's limits. On Windows it also lists the Service
  Control Manager's services. `--ignored` on Linux lists systemd's units,
  and, as root, starts, restarts, stops, masks and unmasks a throwaway unit
  it makes in `/run/systemd/system` and removes.
- `npm test` in `dashboard` covers sorting, search, the tree, ending a tree,
  the service filters and actions, and the tool permissions.
