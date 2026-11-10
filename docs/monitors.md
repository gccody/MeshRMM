# Monitors and self-healing

Monitors are [alert rules](alerts.md) whose condition the device's Agent
checks itself: a service or process, the event log, another computer on the
device's network, or a toolbox script. The Agent runs each one on its own
schedule and tells the server what it found; the server raises and resolves
the rule's alerts as it does for every other rule.

Self-healing is what a rule does about its alerts until they clear: restart
the service, end the process, or run a shared toolbox script. Any rule can
heal itself except device offline, script failed, update install failed and
application install failed ones, whose alerts a person has to look at.
Application updates pending can: its alert clears once the updates are
installed, so a script that installs them heals it.

## Monitor conditions

Each monitor rule has a condition below, an interval (30 seconds to 24
hours, 5 minutes by default), a time limit for one check (1 to 300 seconds,
or 10 to 3,600 for a script check), and how many failed checks in a row
raise its alert (1 to 100). A check that passes resolves the alert
(resolution `cleared`, or `remediated` when self-healing fixed it).

| Condition | Fails when |
|---|---|
| Service not running | the service isn't installed or isn't running, or, if asked, isn't set to start automatically |
| Process check | a process of that name isn't running; or is running; or, all of that name together, uses more than N% of the device's CPU (1 to 99) or N MB of memory for at least M minutes |
| Event log entries | an entry matching the filters was logged since the previous check (on the first check, within the last interval) |
| Ping fails | the host doesn't answer an ICMP echo within the time limit |
| Port not answering | a TCP connection to the host's port fails within the time limit |
| Website check fails | an `http://` or `https://` request doesn't answer with the expected status (200 by default), or its page (the first MB) lacks the keyword; redirects aren't followed, so a 301 is what is checked; a certificate the device doesn't trust is named in words (expired, not yet valid, self-signed or from an unknown authority, for another name), with TLS's own error as the output |
| Certificate expiring | the certificate a TLS server presents has expired or expires within N days (1 to 365) |
| Script check fails | a shared toolbox script exits with a code other than 0, can't run, or runs out of time |

Each result carries a sentence (`detail`), a value where one makes sense
(milliseconds for ping, ports and websites, days left for a certificate, the
load for a process, matching entries for the event log, the exit code for a
script), and output: the matching entries for an event log check, and the
script's standard output and error (the first 16 KB) for a script check. An
alert shows the latest failure's sentence and output.

### On each operating system

| | Windows | Linux | macOS |
|---|---|---|---|
| Service | the Service Control Manager, by service name or display name; automatic means the Automatic start type (delayed or not) | `systemctl show`: running is `active`; automatic is `enabled`, `static`, `generated`, `indirect` or an alias | `launchctl print system/<label>`, launchd's system domain only; automatic means not disabled |
| Process | a process snapshot, by file name with or without `.exe`, ignoring case | `/proc`: the command name or the program's file name | `ps`: the program's file name or path |
| Event log | the named log through the Event Log API; the source matches the provider; event IDs and levels filter; at most 1,000 matching entries a check | the journal (`journalctl`), at most the newest 5,000 entries of the source and levels a check; the source matches the unit (with or without `.service`), syslog identifier or command exactly, as given or in lower case, and an entry shows the one that matched (else its syslog identifier); event IDs don't apply | the unified log (`log show`); the source matches the subsystem, process or sender; event IDs don't apply |
| Ping | the ICMP API, IPv4 and IPv6 | `ping` | `ping`, or `ping6` for an IPv6 address |

The event log levels map onto the journal's priorities (critical is 0 to 2,
error 3, warning 4, information 5 and 6) and the unified log's types (fault,
error, default and info). With no level chosen, every level but debug counts.
An event log rule that names no Windows log isn't sent to Windows devices.
A check that reads as many entries as it may says so in its result ("At
least N matching events…"), since older entries since the last check went
unread. A service monitor is sent to a Linux device only if its name is a
systemd unit name (letters, digits and `:-_.@\`), and to a Mac only if it is
a launchd label (letters, digits and `.-_`); the device's Monitors tab says
why one isn't, and the Agent checks the same again.
A script check runs only on the devices whose operating system has its
interpreter, as a toolbox run does. CPU is measured over one second and is a
share of all the device's cores, as Task Manager shows it, not of one core.

The Agent passes names and hosts to the system as arguments, never through a
shell. The server, when a rule is saved, and the Agent, before each check,
both refuse names that a tool could read as an option (starting with `-`) or
that hold control characters, process names with a folder, sources and log
names with quotes or `< > &` (they become part of the Windows Event Log's
XPath query; the unified log's predicate quotes and escapes them), and hosts
that aren't a host name or IP address. Unit and launchd label rules depend on
the device's operating system, so the server applies them when it decides
which monitors a device gets, as above.

## How it runs

`server/src/monitors/` holds the server's side; `agent/src/remote/monitors/`
the Agent's.

- **Distribution.** When an Agent connects, the server sends it the monitors
  that apply to it (`set_monitors`, with each rule's ID, interval, time limit
  and check; a script check carries the script's language and body). The
  engine's task (`monitors::spawn`) sends a device its set again whenever it
  changes: after a rule, its targets, a device's company or tags, a script,
  or a rule owner's access changes. A presence change (an Agent connecting
  or disconnecting, a device tagged, moved or deleted) looks again at the
  devices it names only, reading just their rows, collected over 250 ms so a
  device that flaps is looked at once. Every connected device is compared
  when a rule is saved, when a tag is deleted (it leaves the rules that
  target it, which may then cover other devices), when the task fell behind
  the presence events, when more than 200 devices change at once, and every
  minute; such full passes run at most once per 5 seconds, with the devices
  named in between still looked at one by one. A device that never had
  monitors gets none sent.
- **Scheduling.** The Agent runs its monitors on a thread of their own, off
  the connection's loop, each within its first interval (at most 30 seconds
  in) and then up to a tenth of the interval (at most 30 seconds) late, so a
  fleet doesn't check at once. At most 8 checks run at a time, 2 of them
  script checks. A check that overruns its time limit by 5 seconds counts as
  failing, and the monitor doesn't start again until it finishes. Monitors
  keep running while the Agent is reconnecting.
- **Reporting.** The Agent reports at once a failure (every one, with its
  count), a change between passing and failing, a monitor's first result
  (also after its definition changes), the result Check now asks for, and
  each self-healing action's outcome. A pass of a monitor the server already
  holds as passing isn't sent as it comes: every 15 minutes
  (`MONITOR_REFRESH_INTERVAL_SECONDS`) the Agent sends the latest results the
  server doesn't hold yet together, usually in one message, starting at a
  random point in the interval on each connection so a fleet's refreshes
  spread out. So a passing monitor's last check time and value on the
  website can be up to 15 minutes old (longer only while the device is
  offline); failures and recoveries show at once.
- **Reconnecting.** The monitors the server sends an Agent when it connects
  (`set_monitors`) carry `known`: what the server holds of each one's latest
  result (when it was checked, whether it passed, and its failures in a
  row). The Agent waits for them (at most 10 seconds), then sends only the
  results the server lacks or holds with another outcome or count, such as
  ones it sent as the old connection dropped. A monitor still passing as the
  server holds it sends nothing until the next refresh. Results the socket
  refuses go again 5 seconds later.
- **Judging.** The Agent counts failures in a row; the server stores each
  monitor's latest result per device (`monitor_states`), raises the rule's
  alert once the count reaches the rule's threshold, keeps it describing the
  latest failure, and resolves it on a pass. It takes a message's results
  together: it matches them against the monitors it last sent the device
  (reading the device's row, tags and platform only if it sent none on this
  connection), keeps them in one statement, and reads the device's
  unresolved alerts of those rules once. A message of passes with no alert
  open costs those two statements and changes no alert, however many
  results it carries. A result for a monitor the device wasn't sent is
  dropped and the device gets its monitors again.
- **Offline devices** report nothing, so their monitor alerts stay as they
  are until the device reconnects and checks again.

A rule raises at most one unresolved alert per device. Changing what a
rule's monitor checks forgets its stored results.

## Self-healing

A rule's self-healing has an action, at most this many attempts per alert
(1 to 10, 3 by default), and the minutes between an attempt finishing and
the next (1 to 1,440, 5 by default):

- **Restart the service**, for a service monitor: on Windows it stops the
  service (failing if other running services depend on it) and starts it; on
  Linux `systemctl restart`; on a Mac `launchctl kickstart -k`. The attempt
  succeeds only if the service manager still says the service runs 3
  seconds later, since a service that crashes at once can look started (a
  systemd simple unit, a job `launchctl` just spawned, or a Windows service
  that stops right after starting).
- **End the process**, for a process monitor that alerts on a running or
  busy process: every process of that name, with `TerminateProcess` on
  Windows, or `SIGTERM` and, after five seconds, `SIGKILL`.
- **Run a shared script**, for any rule that may heal: a toolbox run (source
  `remediation`) as SYSTEM or root, or as the signed-in user.

The Agent never restarts its own service or ends itself, and refuses to end
processes the operating system can't do without (such as `csrss`, `lsass`,
`svchost`, `launchd`, `systemd` and `WindowServer`).

When the rule raises an alert, its first attempt starts at once. When an
attempt finishes, the device checks the monitor again at once (other rules
are judged again), and the next attempt is due after the wait. If the
condition passes within that wait (and a minute more) after an attempt that
succeeded, the alert resolves as `remediated`. If it passes otherwise, after
an attempt that failed, was lost or was skipped, or after self-healing gave
up, it resolves as `cleared`, as without self-healing, and its self-healing
state stays as it was (`failed` stays `failed`). For a rule whose condition
needs a while to pass, such as a load averaged over minutes, choose a wait at
least that long. After the last attempt and one more wait, an alert still
unresolved has failed self-healing.

A device that's offline when an attempt is due is looked at again a minute
later without using an attempt; one that goes offline just as the attempt is
sent fails that attempt. A built-in action the Agent never answers is `lost`
after five minutes, well past its slowest restart (a minute to stop and one
to start a Windows service, two for `systemctl` and `launchctl`); if the
Agent still says it succeeded after that, the attempt is recorded as
succeeded and the device checks again. A script attempt is lost with its run,
which is lost as any toolbox run is (its time limit and two minutes), such as
when the device restarts during it; the next attempt or giving up follows as
after any attempt. A script run that fails counts for
[script failed](alerts.md#rules) rules like any other run. Adding
self-healing to a rule acts on the alerts it raises from then on; taking it
off stops it on the rule's open alerts.

**Problems that keep coming back.** Once self-healing has fixed a rule's
alerts on a device 3 times in 24 hours, it doesn't try on the next one there:
that alert records a `skipped` attempt saying why, its self-healing fails at
once, and its channels hear of it as `alert.remediation_failed`, whether or
not the rule waits for self-healing to fail, with the reason in the
notification (`remediation_stopped` in a webhook). That one notice carries
the alert and stands in for its `alert.raised`, so no channel hears of a
failure before the alert. It heals again once fewer
than 3 fixes fall within the past 24 hours. These limits are fixed
(`REPEAT_FIXES` and `REPEAT_WINDOW_MS` in
`server/src/monitors/remediation.rs`).

The alert's `remediation_state` is `waiting`, `running`, `failed` or
`succeeded`, with `remediation_attempts`; an alert that resolves while
waiting or running, other than as `remediated`, has none. Every attempt is
recorded
(`alert_remediations`: the action, its target, how it ran, its exit code,
detail and output, and when) and shown on the alert, and in the audit log as
`alert.remediate`; a script attempt's run is also recorded as `script.run`.

**Notifications.** A rule notifies its channels when it raises an alert, as
usual, or, if it says so (`notify_when: remediation_failed`), only once
self-healing has failed, as `alert.remediation_failed`, so the problems it
fixes stay quiet. That notice says how the last attempt ended (its outcome,
detail and any exit code; `last_attempt` in a webhook). Saving such a rule so that it notifies at once, or without
self-healing, tells its channels about its open alerts they haven't heard of
yet. A resolution is announced, when the rule asks for it, only for an alert
its channels heard about.

### Whose access it runs with

Some checks and all self-healing run with the access of the user who last
saved the rule, like an [automation policy](automation.md). Those checks are
the ones that reach past the device's own services and processes: a script
check, ping, port, website and certificate checks (which probe hosts from
inside the device's network, and a website keyword check can tell whether a
page holds a word), and event log checks (which read the device's logs).
Service and process checks only look at the device, and need only
`alerts.manage`.

- Saving a rule with one of those checks or with self-healing needs
  `alerts.manage` and `scripts.run`, and so does an API token doing it
  (a token limited to fewer permissions is refused). Rules run only shared
  scripts, and a script a rule runs can't be deleted or unshared until the
  rule changes (`script_in_use`).
- Each attempt checks that the owner is still enabled and holds both
  permissions, and that the script is still shared. If not, the attempt is
  recorded as `skipped` with the reason, self-healing fails, and the rules
  page says why until someone saves the rule with their own access.
- A check that runs with its owner's access stops being sent to devices once
  the owner loses that access (or its script is deleted or unshared), and its
  open alerts, which could no longer change or clear, resolve as
  `not_applicable` at the next evaluation. The device's Monitors tab and the
  rules page say why. Service and process monitors keep watching, since they
  need no one's access; only their self-healing stops.

- Taking power away needs no `scripts.run`: with `alerts.manage` alone, a
  person or token can turn such a rule off (saving it otherwise unchanged; it
  keeps its owner) or delete it, and resolving or acknowledging its alerts
  needs only `alerts.respond`, as for any rule. Changing such a rule in any
  other way, or turning it back on, needs `scripts.run` too, and makes the
  person who does it the rule's owner, as saving it does.

So nobody without `scripts.run` can get scripts run, devices acted on, or
the device's network probed through a rule: an `alerts.manage` user without
it can create service and process monitors and device-state rules only. The
rule editor marks the other conditions as needing `scripts.run`.

## In the website

- The rule editor's monitor conditions show their check's fields, the
  interval, time limit and failures in a row, and the **Self-healing**
  section (for anyone holding `scripts.run`).
- **Rules** says what each rule's self-healing does, whose access it runs
  with, and any problem.
- An alert shows its output and, behind a disclosure, its self-healing
  attempts.
- A device's **Monitors** tab (`/device?id=…&tab=monitors`, with
  `alerts.view`) lists the monitors that apply to it with what each last
  found, when (for a passing monitor, as of the Agent's last refresh, up to
  15 minutes ago), its output and alert, why one isn't checked there, and
  **Check now** (with `alerts.respond`), which updates live.

## API

| Route | Permission | |
|---|---|---|
| `GET`, `POST /v1/alert-rules`; `PUT /v1/alert-rules/{id}` | `alerts.view`; `alerts.manage` (and `scripts.run` for self-healing, or a script, ping, port, website, certificate or event log check, except to turn such a rule off) | A monitor rule's `monitor`: `interval_seconds`, `timeout_seconds`, `failures` and `check` (`kind` and its fields, as in `crates/protocol-types/src/monitors.rs`). Any rule's `remediation`: `action` (`restart_service`, `end_process` or `script`), `script_id` and `run_as` for a script, `attempts` and `retry_minutes`; and `notify_when` (`raised` or `remediation_failed`). Rules list with `remediation_description`, `owner` and `problem`. |
| `GET /v1/agents/{id}/monitors` | `devices.view` and `alerts.view` | The monitors that apply to the device, each with its latest `state`, unresolved alert and why the device doesn't run it (`problem`) |
| `POST /v1/agents/{id}/monitors/check` | `devices.view` and `alerts.respond` | `{ "rule_ids": [...] }`, or every monitor when empty; `202`, `404` for a monitor the device doesn't run, `409 device_offline`; audited as `monitor.check` |
| `GET /v1/monitor-states?rule=` | `alerts.view` | A monitor's latest result on each device that reported one, failing first |
| `GET /v1/alerts/{id}/remediations` | `alerts.view` | The alert's attempts, first first |

Personal API tokens use these routes as their user. The event socket sends
users with `alerts.view` a `monitor_update` (the device and its new results)
whenever a device reports, besides the alert events.

## Tables

`alert_rules` carries `monitor_check` (the check as JSON) and its schedule,
`check_script_id`, the `remediation_*` columns and `notify_when`; `alerts`
carries `output`, `notified` and the `remediation_*` columns;
`monitor_states` holds each monitor's latest result per device and
`alert_remediations` each attempt.

## Tests

- `cargo test -p meshrmm-protocol-types` covers validating checks.
- `cargo test -p meshrmm-server --lib` covers the monitor and self-healing
  settings and their wording, and turning script runs into attempts.
- `cargo test -p meshrmm-server --test monitors` runs on SQLite and
  PostgreSQL: monitors reaching Agents on connect and after changes, with
  what the server holds of their results and for how many statements,
  results raising and resolving alerts with thresholds, a message of
  results kept and judged together (a refresh costing two statements
  however many it carries, and failures and recoveries in one message
  raising and resolving alerts as one by one), Check now, targets and
  platforms, restarting a service and resolving as `remediated`, attempts
  waiting their turn and failing with a notification, lost actions and late
  answers, lost script runs, script self-healing for a load rule, `cleared`
  rather than `remediated` after failed or skipped attempts, alerts resolved
  during an attempt, taking self-healing off a quiet rule, stopping for a
  problem that keeps coming back, permissions (including for probes and
  limited tokens), owners losing access, and API tokens.
- `cargo test -p meshrmm-agent monitors` covers the scheduler, which results
  go at once and which wait for the refresh, what a reconnect sends given
  what the server holds, results the socket refused, every check's
  judging, and reading each tool's output from captures of the real tools
  (`agent/src/remote/monitors/testdata/`): Windows Event Log XML, journal
  JSON, `systemctl show`, `/proc/<pid>/stat`, `ping` and certificates.
  On Windows it also reads the Service Control Manager, processes, the
  System log and pings; `--include-ignored` on Linux reads systemd, the
  journal and `ping`.
- `npm test` in `dashboard` covers the monitor and self-healing forms,
  which conditions need `scripts.run`, results on the event socket and their
  statuses.

## Known gaps

- macOS services are launchd's system domain only; per-user launch agents
  can't be monitored or restarted.
- macOS pings an IPv6 address with `ping6`, which has no overall time limit,
  so a silent host counts as not answering when the check's time limit ends;
  a host name that resolves only to IPv6 can't be pinged on a Mac.
- Event IDs filter only on Windows; the journal and unified log have none.
- A certificate check reads when the server's certificate expires; it
  doesn't check that the device trusts it (a website check does, unless told
  not to).
- Windows service restarts don't stop dependent services first.
