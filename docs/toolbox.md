# Toolbox

The toolbox keeps the scripts and files a technician uses on devices, much
like ScreenConnect's. Scripts are written and edited on the dashboard's
**Toolbox** page. They run on a device from the dashboard, or from the
viewer's toolbox while connected. Library files are uploaded on the same page,
and the viewer's toolbox sends them to the connected device.

Each script and file is private to the user who added it, or shared with the
whole company. Users see their own items and everything shared. The owner can
change or delete an item, and so can a company administrator while it is
shared. A private item stays private, even from administrators. Anyone in the
company can run a shared script or send a shared file.

## Scripts

A script has a name, an optional folder and description, an interpreter
(PowerShell or Command Prompt), a timeout from 10 to 3600 seconds (300 by
default), and its source, up to 128 KiB. Folders are names separated by `/`,
up to eight levels deep, so `Maintenance/Disk` nests `Disk` in `Maintenance`.

Each run chooses the account:

- **Signed-in user** runs the script as the user signed in to the console, or
  to an active Remote Desktop session when nobody is at the console. It runs
  with their limited (unelevated) token and environment, on their desktop.
  When nobody is signed in, it runs as SYSTEM instead, and the result says so:
  `NT AUTHORITY\SYSTEM (nobody was signed in)`.
- **SYSTEM** runs it as the Agent's LocalSystem account in Session 0.

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
  runs; members see their own and administrators see everyone's. Runs and
  their output are kept for 30 days.
- **Viewer:** open the toolbar's toolbox button (beside Files), choose a script
  in its folder, then **As the signed-in user** or **As SYSTEM**. While it
  runs, the button is highlighted and its tooltip says so. When it finishes, a
  window opens with the outcome and output, which can be selected and copied.
  Runs from the viewer also appear in the dashboard's run history.

## Files

Library files are uploaded from the browser, up to 95 MiB each, into a folder,
private or shared. A file's name must be one Windows allows. The browser
computes each file's SHA-256, R2 refuses an upload whose content does not
match, and the Agent checks it again before saving. Files can be renamed,
moved to another folder, shared or unshared, downloaded, and deleted. Their
content cannot change; upload a new file instead.

In the viewer, the toolbox's **Send a file to Documents** section lists the
library by folder. Choosing a file sends it to the connected device:

- Viewing a user's session: the signed-in user's
  `Documents\MeshRMM Transferred Files`, where the viewer's own file transfers
  go.
- On the background desktop: `C:\Users\Public\Documents\MeshRMM Transferred
  Files`. Public Documents is what the background desktop's File Explorer
  shows as Documents.
- With nobody signed in: Public Documents too.

A file with the same name gets a number, as in `setup (2).exe`. The toolbar
tooltip and the toolbox menu show where the file was saved; a failure opens an
error message.

## Data path

```text
Dashboard (WorkOS token) ─┐                 ┌─ Viewer (session client token)
  /v1/toolbox/...         │                 │  /v1/remote/sessions/{id}/toolbox
  /v1/agents/{id}/script-runs               │  /v1/remote/sessions/{id}/script-runs
                          ▼                 ▼  /v1/remote/sessions/{id}/file-deliveries
                        Worker ── D1: toolbox_scripts, toolbox_files,
                          │           script_runs, file_deliveries
                          │       R2: toolbox/{company_id}/{file_id}
                          ▼
            AgentCoordinator /command ──WebSocket──▶ Agent coordinator (Session 0)
                                                   run_script / deliver_file
                                                     own thread per job
            Agent ──HTTPS (Agent credential)──▶ Worker
              GET  /v1/agents/{id}/file-deliveries/{delivery}/content
              POST /v1/agents/{id}/script-runs/{run}/result
              POST /v1/agents/{id}/file-deliveries/{delivery}/result
```

Nothing crosses the WebRTC session, so the toolbox works on the background
desktop and with nobody signed in, where the viewer's own file transfers
cannot. The viewer's toolbox acts for the dashboard user who started the
session: the session record keeps their user ID, and the remote-session
Durable Object confirms the viewer's client token before the Worker serves
that user's toolbox. A run or delivery starts only on the company's own
undeleted, connected device; an offline device fails it at once.

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
- An Agent runs at most 8 scripts and 4 downloads at once and refuses more.
  Reports are retried for about a minute while the server is unreachable.

Every script change, upload, run and delivery is recorded in the company's
audit events (`toolbox.script_*`, `toolbox.file_*`, `script.run`,
`file.deliver`). The scheduled maintenance task deletes runs and deliveries
older than 30 days.

## Setup

Create the bucket once before deploying the server:

```sh
cd server
npx wrangler r2 bucket create meshrmm-toolbox
```

`server/wrangler.jsonc` binds it as `TOOLBOX`. Apply migration
`0019_toolbox.sql` before deploying the server and dashboard; `/healthz`
expects it. Agents and viewers need a release with the toolbox; older Agents
ignore toolbox commands, and their runs show **No result**.

## Validation

- `cargo test -p meshrmm-protocol-types`, `-p meshrmm-server`,
  `-p meshrmm-agent` and `-p meshrmm-remote` cover validation, the run and
  delivery helpers, the toolbox menu and the output report.
- `python3 server/tests/sql_regressions.py` covers who sees and changes which
  items, one report per run, and which deliveries an Agent may download.
- `node server/tests/toolbox.mjs`, after `worker-build`, runs the Worker under
  Miniflare: private and shared items, administrator edits, checked uploads,
  runs and deliveries through a connected Agent from the dashboard and from a
  session, and Agent reports.
- `npm test` in `dashboard` covers the toolbox's form rules and labels.
