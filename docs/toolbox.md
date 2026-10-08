# Toolbox

The toolbox keeps the scripts and files a technician uses on devices, much
like ScreenConnect's. Scripts are written and edited on the dashboard's
**Toolbox** page. They run on a device from the dashboard, or from the
viewer's toolbox while connected. Library files are uploaded on the same page,
and the viewer's toolbox sends them to the connected device.

Each script and file is private to the user who added it, or shared with
everyone. Users with `scripts.run` keep their own private scripts and run
those and the shared ones; `files.deliver` does the same for files. Sharing an
item, and changing or deleting a shared one, needs `scripts.manage_shared` (or
`files.manage_shared`). A private item stays private, even from
administrators.

## Scripts

A script has a name, an optional folder and description, an interpreter
(PowerShell or Command Prompt for Windows, Shell (zsh) for Macs), a timeout from 10 to 3600 seconds (300 by
default), and its source, up to 128 KiB. Folders are names separated by `/`,
up to eight levels deep, so `Maintenance/Disk` nests `Disk` in `Maintenance`.

Each run chooses the account:

- **Signed-in user** runs the script as the user signed in to the console, or
  to an active Remote Desktop session when nobody is at the console. It runs
  with their limited (unelevated) token and environment, on their desktop.
  When nobody is signed in, it runs as SYSTEM instead, and the result says so:
  `NT AUTHORITY\SYSTEM (nobody was signed in)`.
- **SYSTEM** runs it as the Agent's LocalSystem account in Session 0.

On a Mac, the signed-in user is the one at the console, and the script runs
in their login session with their home folder as its working folder; the
system account is root, with `/` as the working folder. A device refuses a
script for the other operating system's interpreters.

The script's input is empty, so a prompt for input ends or fails instead of
waiting. When the timeout passes, the Agent stops the script and everything it
started, and reports **Timed out** with the output so far. Programs a script
starts in the background keep running after the script exits normally.

The result records the account the script ran as, its exit code, and its
standard output and error, up to 512 KiB each. Both interpreters are asked to
write UTF-8: PowerShell through `[Console]::OutputEncoding`, Command Prompt
through `chcp 65001`. Output a program writes in the OEM code page is decoded
as such. A run the device never reports on, for example because it went
offline or restarted, shows **No result** two minutes after its timeout.

### Running a script

- **Dashboard:** choose **Run** on a script in the Toolbox, **Run a script** at
  the top of it, or the terminal button on a device's row on the Devices page.
  Pick the script, an online device, and the account. The dialog follows the
  run and shows its output when it finishes. **Run history** lists the last 50
  runs; users see their own, and users with `audit.view` see everyone's. Runs
  and their output are kept for 30 days.
- **Viewer:** open the toolbar's toolbox button (beside Files), choose a script
  in its folder, then **As the signed-in user** or **As SYSTEM**. While it
  runs, the button is highlighted and its tooltip says so. When it finishes, a
  window opens with the outcome and output, which can be selected and copied.
  Runs from the viewer also appear in the dashboard's run history.

## Files

Library files are uploaded from the browser into a folder, private or shared,
up to `toolbox.max_file_bytes` each (95 MiB by default; `GET /v1/toolbox`
reports it). A file's name must be one Windows allows. The browser computes
each file's SHA-256, the server refuses an upload whose content does not
match, and the Agent checks it again before saving. Files can be renamed,
moved to another folder, shared or unshared, downloaded, and deleted. Their
content cannot change; upload a new file instead.

The website sends a file to a device with `POST /v1/agents/{id}/file-deliveries`,
choosing the signed-in user's folder or Public Documents. In the viewer, the
toolbox's **Send a file to Documents** section lists the library by folder.
Choosing a file sends it to the connected device:

- Viewing a user's session: the signed-in user's
  `Documents\MeshRMM Transferred Files`, where the viewer's own file transfers
  go.
- On the background desktop: `C:\Users\Public\Documents\MeshRMM Transferred
  Files`. Public Documents is what the background desktop's File Explorer
  shows as Documents.
- With nobody signed in: Public Documents too.

On a Mac, the user's file goes to `~/Documents/MeshRMM Transferred Files`,
written as the user, and the background or nobody-signed-in file goes to
`/Users/Shared/MeshRMM Transferred Files`, which the Agent uses only while
it owns that folder.

A file with the same name gets a number, as in `setup (2).exe`. The toolbar
tooltip and the toolbox menu show where the file was saved; a failure opens an
error message.

## Data path

```text
Website (session cookie) ─┐                 ┌─ Viewer (session client token)
  /v1/toolbox/...         │                 │  /v1/remote/sessions/{id}/toolbox
  /v1/agents/{id}/script-runs               │  /v1/remote/sessions/{id}/script-runs
  /v1/agents/{id}/file-deliveries           │  /v1/remote/sessions/{id}/file-deliveries
                          ▼                 ▼
                        server ── database: toolbox_scripts, toolbox_files,
                          │                 script_runs, file_deliveries
                          │       data_dir: toolbox/{file_id}
                          ▼
            Agent control connection ──WebSocket──▶ Agent coordinator (Session 0)
                                                   run_script / deliver_file
                                                     own thread per job
            Agent ──HTTPS (Agent credential)──▶ server
              GET  /v1/agents/{id}/file-deliveries/{delivery}/content
              POST /v1/agents/{id}/script-runs/{run}/result
              POST /v1/agents/{id}/file-deliveries/{delivery}/result
```

Nothing crosses the WebRTC session, so the toolbox works on the background
desktop and with nobody signed in, where the viewer's own file transfers
cannot. The viewer's toolbox acts for the user who started the session: the
session record keeps their user ID, and the server checks the viewer's client
token before serving that user's toolbox with their current permissions. A run or delivery starts only on an
undeleted, connected device; an offline device fails it at once.

Uploads are written to a hidden partial file in `toolbox/`, checked, flushed
and renamed into place, so a file is never visible half-written. Deleting a
file removes its row first, then its content. The maintenance task removes
partial files a stopped server left behind.

On the Agent:

- Script files live in a new folder under
  `%ProgramData%\MeshRMM\Agent\scripts\{run_id}` that only SYSTEM and
  administrators can change. A script that runs as the user can read and
  execute its folder but not change it. The folder is deleted after the run.
- Downloads are staged in `%ProgramData%\MeshRMM\Agent\deliveries` and checked
  against their size and SHA-256 before they are saved. While a user is signed
  in, the Agent creates the transfer folder and the file with their token, so
  the user owns them and a folder they control cannot redirect a SYSTEM write.
  The Agent never writes through a transfer folder that is a link or junction,
  and removes a file whose final path shows the folder was swapped for one
  while it was being saved.
- On a Mac, a script is written to a new folder in the temporary folder that
  only the account it runs as can read, and zsh runs it in its own process
  group, which a timeout stops as a whole. Downloads are staged in the
  Agent's root-only support folder.
- An Agent runs at most 8 scripts and 4 downloads at once and refuses more.
  Reports are retried for about a minute while the server is unreachable.

Every script change, upload, run and delivery is recorded in the audit log (`toolbox.script_*`, `toolbox.file_*`, `script.run`,
`file.deliver`). The scheduled maintenance task deletes runs and deliveries
older than 30 days.

## Setup

Nothing to set up: the server creates `toolbox/` in its data directory. Back
it up with the database; a file whose content is lost can't be downloaded or
delivered. Behind a proxy that limits request bodies, keep
`toolbox.max_file_bytes` under its limit.

## Validation

- `cargo test -p meshrmm-protocol-types`, `-p meshrmm-agent` and
  `-p meshrmm-remote` cover validation, the run and delivery helpers, the
  toolbox menu and the output report.
- `cargo test -p meshrmm-server --test toolbox` runs on SQLite and PostgreSQL:
  private and shared items and who may change them, checked uploads,
  downloads, runs and deliveries through a connected Agent, Agent reports,
  and lost runs and deliveries.
- `npm test` in `dashboard` covers the toolbox's form rules and labels.
