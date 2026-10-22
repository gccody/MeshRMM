# Remote terminal

A live, interactive shell on a device, in the browser, without a remote
session. It is the way into headless Linux servers, which have no desktop to
view, and a quick one into any other device.

## Opening a terminal

**Terminal** on an online device's tile (when the pointer is over it, and in
its **⋯** menu) and on the device's page opens the device's terminal page,
`/terminal?id=…`, in a new browser tab. Choose a shell and an account, then
**Open**. **New terminal** opens another on the same device in a tab of the
page; each browser tab or window can hold another device's terminals, so
several stay open at once.

The shells the device has:

| | Windows | macOS | Linux |
|---|---|---|---|
| Shells | Windows PowerShell, Command Prompt, PowerShell 7 where installed | The account's login shell, bash, zsh, sh | The account's login shell, bash, zsh, sh, where installed |

The Agent lists them in the device's [system information](hardware-and-software.md)
(`os.terminal_shells`). Until it has reported them, the website offers
Windows PowerShell and Command Prompt, or the login shell and sh.

The account is one of:

- **SYSTEM** (Windows) or **root** (macOS, Linux): the Agent's own account.
  On Windows the shell runs in Session 0 with System32 as its working folder;
  on macOS and Linux in root's home folder.
- **Signed-in user**: the user at the console, or on Windows in an active
  Remote Desktop session when nobody is at the console, the same user a
  script picks (see [toolbox](toolbox.md#scripts)). On Windows the shell gets
  their limited (unelevated) token, their environment and their profile
  folder, in their session. On a Mac it is a login through `login`, so it
  gets the user's launchd context (their keychain, `open`, the clipboard). On
  Linux it runs with the user's groups, their home folder and the session's
  `XDG_RUNTIME_DIR` and D-Bus address. When nobody is signed in, as on a
  server, the terminal doesn't open and says so. Unlike a script it never
  falls back to SYSTEM or root.

On macOS and Linux the shell is a login shell (its `argv[0]` starts with
`-`, so it reads `/etc/profile`, `~/.zprofile` and the like) in a new session
with the pseudo-terminal as its controlling terminal, and starts with `HOME`,
`USER`, `LOGNAME`, `SHELL`, `PATH`, `LANG` (the Agent's, or `C.UTF-8` on Linux
and `en_US.UTF-8` on a Mac without one), `TERM=xterm-256color` and
`COLORTERM=truecolor`. On Windows it runs on a pseudoconsole (ConPTY), which
turns the console into the same VT sequences; programs see a normal console.

## Using it

The terminal is [xterm.js](https://xtermjs.org): colours, full-screen programs
(`top`, `htop`, `vim`, `nano`), the mouse where a program asks for it, and
line editing and tab completion as the shell provides them. It follows the
size of the browser window. **Ctrl+C** goes to the shell; copy with
**Ctrl+Shift+C** (**⌘C** on a Mac) or the **Copy** button, and paste with
**Ctrl+Shift+V** (**⌘V**) or **Paste**. A paste is limited to 1 MiB.

The status line shows **Connecting…**, **Connected** with the account and
the shell's program, **Disconnected** with the reason, or **Exited** with
the exit code. A terminal that ended can't be resumed: **Reconnect** opens a
new shell with the same choices in the same tab.

Typing in a terminal counts as using the website, so the website's idle
sign-out doesn't end a terminal in use.

## When a terminal ends

A terminal lasts as long as its browser socket. It ends, and the Agent stops
the shell and everything it started, when:

- the user closes its tab, the page, or the browser, or the browser loses its
  connection;
- the shell exits;
- nobody has typed in it for the company's idle time: **Settings →
  Terminals → Close idle terminals after** (5 minutes to 8 hours, 30 minutes
  by default). Output doesn't count, so a `tail -f` left alone is closed too;
- the user signs out, is signed out, or loses the permission the terminal
  needs (checked every 30 seconds and whenever a signed-in user changes
  something);
- the device goes offline, or its Agent restarts or updates. Automatic Agent
  updates wait while a terminal is open, as they do for remote sessions.

On Windows the shell runs in a job object that kills every process in it when
the terminal closes. On macOS and Linux every process in the shell's session
gets `SIGHUP`, and whatever is still running two seconds later `SIGKILL`.
Programs that leave the session on purpose, such as daemons and anything
started with `setsid`, keep running, as they would after an SSH session.

## Limits

- 5 terminals per device and 10 per user at once, counting those still
  opening; an Agent runs at most 8.
- 1000 columns by 500 rows.
- The browser has 30 seconds to connect after asking for a terminal, and the
  Agent 20 seconds to answer.

## Permissions

| Permission | Allows | Technician |
|---|---|---|
| `terminal.connect` | Terminals as the signed-in user | Yes |
| `terminal.connect_system` | Terminals as SYSTEM or root | Yes |

A terminal as SYSTEM or root can do anything on the device, as much as a
script run as SYSTEM (`scripts.run`) or a [background session](background-mode.md)
(`sessions.connect_background`), which the Technician role also has, so it
has both terminal permissions by default. Take `terminal.connect_system` out
of a role to give it shells only as the signed-in user, which are limited to
what that user may do; on Windows that is an unelevated token, so even an
administrator's terminal can't answer a UAC prompt.

## What the computer's user sees

Terminals follow the company's [connection notification](maintenance-controls.md#connection-notification)
policy, with its message:

- A terminal as the **signed-in user** runs in their session, like a remote
  session on their desktop, so it shows the notification when **Notify the
  agent's user when a technician connects to their session** is on (the
  default).
- A terminal as **SYSTEM or root** is as invisible to them as background
  mode, so it shows the notification only when **Also notify the user when a
  technician connects in background mode** is on.

The notification appears for the user signed in to the console, if any, when
the terminal opens. On Windows it is the same notification a remote session
shows; on macOS and Linux it is shown by the Agent's session helper in the
user's session.

[Connection approval](maintenance-controls.md#connection-approval) doesn't
apply to terminals. It asks the user to let a technician see and control
their screen, which a terminal never does, and a terminal is mostly opened on
servers, where nobody could answer. A terminal is as powerful as a toolbox
script, which runs without approval too; control it with the permissions
above, and the audit log records every one.

## Audit

Each terminal is recorded on its device:

- `terminal.open` when the website asks the Agent for it: who, from where,
  `terminal_id`, `shell` and `run_as`.
- `terminal.close` when it ends, with `reason` (`exited`, `failed`, `closed`,
  `idle`, `signed_out`, `permission_removed`, `device_offline`,
  `device_timeout`, `device_disconnected` or `protocol_error`), the
  `account` and `program` the Agent reported, the `exit_code`, the `error` of
  a shell that couldn't start, and `duration_ms`.

What is typed and what the shell writes are never logged or stored.

## How it works

```text
browser ──wss /v1/terminals/{id}──▶ server ◀──wss /v1/agents/{device}/terminals/{id}── Agent
   (session cookie, same origin)       │          (Agent credential)                    │
                                       │                                         pseudo-terminal
                                       └── control connection: open_terminal ──▶   └─ shell
```

1. `POST /v1/agents/{id}/terminals` with `{ "shell", "run_as", "columns",
   "rows" }` checks the permission, that the device is online and has such a
   shell, and the limits, and returns the terminal's `id`. Errors carry the
   codes `permission_denied`, `shell_unsupported`, `invalid_size`,
   `device_offline` and `too_many_terminals`.
2. The browser opens `/v1/terminals/{id}` with its session cookie. Only the
   user who asked may, once, from the website's origin.
3. The server records `terminal.open` and sends the Agent `open_terminal`
   over its control connection, with the company's notification text when
   policy shows one.
4. The Agent starts the shell, connects `/v1/agents/{device}/terminals/{id}`
   with its own credential, and says `started` (with the account and
   program) or `failed`.
5. The server relays between the two sockets until one ends.

Terminal traffic never goes over the Agent's control connection, so a flood
can't delay its metrics, scripts or session requests.

On both sockets binary messages carry the terminal's bytes, input one way and
output the other, untouched; the browser decodes UTF-8 across message
boundaries. Text messages are JSON (`crates/protocol-types/src/terminal.rs`):

| From | Message |
|---|---|
| Browser | `{"type":"resize","columns":120,"rows":40}` |
| Browser | `{"type":"ack","bytes":N}`: the browser has drawn `N` bytes of output so far |
| Agent | `{"type":"started","account":"root","shell":"/bin/bash"}` |
| Agent | `{"type":"failed","message":"…"}`, then the socket closes |
| Agent | `{"type":"exited","code":0}` (`null` when a signal ended it), then the socket closes |

**Flow control.** The Agent sends at most 512 KiB more output than the
browser has acknowledged, and the browser acknowledges output once xterm.js
has drawn it. When the window is full the Agent stops reading the
pseudo-terminal, so a `yes` flood or a large `cat` waits on the device, in
the kernel's buffer, and the program writing it blocks. The server holds at
most one message per direction, relays each direction in its own loop so
input isn't held up behind output, and closes a browser that acknowledges
output it wasn't sent. The Agent never stops reading its socket, so an
acknowledgement can't get stuck behind input the shell isn't reading; it
keeps at most 1 MiB of such input and drops the rest.

The browser socket closes with a code that says why: 1000 when the shell
exited or couldn't start, or the terminal was closed; 1003 for a message a
terminal doesn't take; 4001 sign in again; 4003 the permission was removed;
4004 the device is offline or didn't open the terminal; 4008 idle; 4009 the
device disconnected. Both sides ping every 20 seconds.

Server: `server/src/terminals/` (the registry of terminals and their limits,
and the relay), `server/src/api/terminals.rs`. Agent:
`agent/src/remote/terminal.rs` and its platform modules. Website:
`dashboard/features/terminal/`, loaded only when a terminal page opens.

## Tests

- `cargo test -p meshrmm-protocol-types terminal` covers the wire types.
- `cargo test -p meshrmm-server --test terminals` runs on SQLite and
  PostgreSQL: who may open a terminal, the relay both ways, acknowledgements,
  every way a terminal ends, the limits, the setting and the audit log.
- `cargo test -p meshrmm-agent terminal` starts real shells on a
  pseudo-terminal (ConPTY on Windows), and covers flow control, resizing and
  stopping everything a shell started.
- `npm test` in `dashboard` covers the terminal page's shells, accounts,
  input chunking, acknowledgements and close messages.
