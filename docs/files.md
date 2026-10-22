# File browser

A device's folders and files in the website, without a remote session:
browse, download, upload, and create, rename, move and delete. It reaches
headless servers as easily as desktops.

## Opening it

**Files** on an online device's page, and **Browse files** in the **⋯**
menu of its tile, open the device's file browser, `/files?id=…`, in a new
browser tab. It starts at the device's drives (Windows) or `/` and its
mounted volumes (`/Volumes` on a Mac; `/media`, `/mnt` and `/run/media` on
Linux), with their free space, and its users' folders: the account's own
home, and the Home, Desktop, Documents and Downloads folders of the user
signed in to the computer, wherever Windows redirects them (OneDrive, a
share) and under the names a Linux desktop gives them
(`~/.config/user-dirs.dirs`). The same places stay in the sidebar.

The page's address carries the folder and the account (`&path=…&as=user`),
so a folder can be bookmarked or shared with another technician, and the
browser's Back button goes to the folder before.

## The account

Operations run as **SYSTEM** (Windows) or **root** (macOS, Linux), the
Agent's own account, by default. **Signed-in user** in the page's header
runs them as the user signed in to the computer instead: the console user,
or on Windows the user of an active Remote Desktop session when nobody is at
the console, the same user a script or a terminal picks. When nobody is
signed in, the page says so; unlike a script, it never falls back to SYSTEM
or root.

As the signed-in user, the computer checks every access against that user,
with their groups:

- **Windows** impersonates the user's token, their limited one for an
  administrator, as UAC gives them.
- **Linux** switches one thread to the user's user and group IDs and
  supplementary groups, which also drops root's capabilities.
- **macOS** gives one thread the user's credentials (`pthread_setugid_np`),
  with their groups from the directory service.

Each operation runs on a thread of its own that ends with it, so the user's
identity never leaks into anything else the Agent does. A file opened as
the user stays open with that access while it's transferred.

What root creates on macOS and Linux (folders, uploads, copies) gets the
owner and group of the folder it's in, so a file put in a user's home stays
theirs. On Windows, new entries inherit their folder's permissions.

## Browsing

A folder lists its entries with their name, size, modification time, type
and the hidden, system and read-only flags. Folders come first; the rest
sort by name (naturally, so `file2` comes before `file10`, ignoring case),
size, modification time or type, either way. Hidden entries (a name starting
with `.` on macOS and Linux, the hidden flag on macOS and Windows) and
Windows system files are left out until **Hidden** is on. **Filter this
folder** narrows the listing by name. A link shows what it points to; a
broken link shows as one.

The breadcrumb above the listing leads back up; the pencil beside it turns
it into a box to type or paste any path into. In the listing, a double
click or Enter opens a folder or downloads a file, the arrow keys move,
Space and Ctrl/⌘-click choose several entries and Shift-click a range,
Delete deletes, F2 renames and Backspace goes up. Each row's **⋯** menu has
its actions.

Large folders work as small ones do. The Agent reads a folder once, and the
page asks for the 500 entries around what's on screen, drawing only the rows
in view; the Agent pages, sorts and filters that reading for a minute
without reading the folder again. A folder of 50,000 entries opens in about
a second and scrolls without delay. A folder of more than 250,000 entries
lists its first 250,000 and says so; filter by name to reach the rest.

## Changing files

With `files.manage`:

- **New folder**.
- **Rename**, in the same folder. On a volume that ignores case (Windows,
  most Macs), a rename can change only the case.
- **Move** to a folder whose path you type or pick, one entry or many.
  Nothing is replaced: an entry whose name is taken in the destination stays
  where it is, and the dialog lists it with the reason. A move to another
  volume copies, then deletes the original once the copy is whole (a copy
  that fails is removed and the original kept). Links are moved as links,
  never followed; on Windows a link can't move to another drive.
- **Delete**, after a confirmation that names what goes. As the signed-in
  user the default is the user's Recycle Bin or Trash, where they can find
  it (see [platform parity](platform-parity.md#file-browser) for each
  platform's), with **Delete permanently** as the other choice; an entry
  that can't go there, such as one on a network drive, fails with a
  reason, and the dialog offers deleting it permanently. SYSTEM and root
  have no Recycle Bin or Trash, so as them a delete is permanent, and the
  confirmation says so. A folder is deleted with everything in it; a link
  is deleted itself, never what it points to. Read-only files are deleted,
  as Explorer does once it has asked.
- **Copy path** copies the chosen entry's path, or the folder's.

A move or delete of many entries does what it can and reports each one it
couldn't, such as one in use or denied, so you can try those again.

## Downloads

**Download** saves a file as itself; a folder, or several entries chosen
together, arrive as one ZIP archive the Agent writes while it sends it
(Deflate, fast setting), named after the folder. Links inside a folder are
left out, never followed; files that can't be read (in use, denied) are left
out too, and listed in `MeshRMM - files left out.txt` at the end of the
archive.

The browser saves downloads with its own download manager, so a large file
never sits in the page's memory, and the browser shows its progress. The
**Transfers** list under the folder shows it too, from the server's side,
and once a download is complete, the SHA-256 of what was sent, to compare
with the file on the device or on your computer. If the connection drops,
the browser's **Resume** goes on from where it stopped for a single file,
while the download lasts (12 hours after its last use), and only if the
file hasn't changed on the device since. An archive starts again instead.

The Agent works out the SHA-256 of what it reads as it sends it, and the
server the SHA-256 of what it relays; they and the byte counts must match
at the end, or the server aborts the response, so the browser marks the
download failed rather than keeping a damaged file. A download whose device
goes offline, or whose file shrinks while it's read, fails the same way.

## Uploads

**Upload files** and **Upload folder** pick from your computer, and files
and folders dragged from your computer onto the listing upload into the
folder shown, or onto a folder row into that folder. A folder keeps its
structure, empty folders included when dragged; missing folders on the way
are created.

Each file goes to a hidden partial file beside where it ends up,
`.meshrmm-upload-<id>` (with the hidden flag on Windows), in pieces of 64
MiB, three files at a time. Meanwhile the browser works out the file's
SHA-256, reading it a piece at a time. Once the device has every byte, it
checks the partial file's SHA-256 against the browser's and only then
renames it into place, so a file appears whole or not at all; a mismatch
discards it and says so. Uploaded files get the Mark of the Web on Windows
and the quarantine attribute on macOS, as files received in a remote
session do.

An upload never replaces a file by itself: one whose name is taken waits
with **Replace** and **Skip**. A dropped connection goes on from what the
device has, by itself up to three times; a device that goes offline pauses
its uploads, and they go on when it's back (or with **Resume**). **Cancel**
removes the partial file. The device deletes partial files nobody has added
to for a day. Closing or leaving the page while files are uploading asks
first, since it stops them.

## Limits

- **Settings → Files → Largest transfer** (1 MB to 1 TB, 4 GB by default):
  the largest file a download or upload may carry, and the most a ZIP
  archive may hold. Checked before a transfer starts and again as it
  streams.
- 4 transfers streaming per device and 6 per user at once (a download in
  the browser counts while it streams); the page uploads three files at a
  time. An Agent streams at most 8 transfers and runs 4 other operations
  at once.
- A download can be fetched again, or resumed, for 12 hours after its last
  use, and an unfinished upload resumed for 24 hours; both are kept in the
  server's memory, so a server restart forgets them.
- Paths up to 32 KB; 1,000 entries per move, delete or download.

Transfers stream through the server without being stored there, a piece at
a time. A reverse proxy in front of the server must allow request bodies of
64 MiB and not buffer them (see [self-hosting](self-hosting.md#behind-a-reverse-proxy-proxy)).

## Permissions

| Permission | Allows | Technician |
|---|---|---|
| `files.browse` | Browse folders and download files | Yes |
| `files.manage` | Upload, and create, rename, move and delete files and folders | Yes |
| `files.system` | Do either as SYSTEM or root; without it, only as the signed-in user | Yes |

SYSTEM and root reach every file on a device, including other users' and
the system's own, which is why that takes its own permission: a role with
`files.browse` (and `files.manage`) alone works only as the signed-in user,
limited to what that user may do, and only while someone is signed in. The
Technician role has all three, because it already has `scripts.run` and
`terminal.connect_system`, which reach every file as SYSTEM or root too;
taking `files.system` from it alone wouldn't narrow what it can do. Make a
custom role for read-only or user-limited file access.

API tokens may use every file route, within their permissions and their
user's, so scripts can fetch logs or push files; their actions are recorded
with the token's name.

## What the computer's user sees

The file browser follows the company's
[connection notification](maintenance-controls.md#connection-notification)
policy, as terminals do: working as the **signed-in user** shows the
notification when **Notify the agent's user when a technician connects to
their session** is on (the default), and working as **SYSTEM or root**
only when **Also notify the user when a technician connects in background
mode** is on. It appears with a technician's first operation on a device in
30 minutes, for each account, not with every folder they open.

[Connection approval](maintenance-controls.md#connection-approval) doesn't
apply. It asks the user to let a technician see and control their screen,
which the file browser never does, and it's used mostly on servers, where
nobody could answer. It is as powerful as a toolbox script, which runs
without approval too; control it with the permissions above, and the audit
log records every change and transfer.

## Audit

Browsing isn't recorded; everything that changes or copies files is, on the
device, with `run_as`:

- `files.download`, when a download's stream ends: the `paths`, the `file_name`,
  `kind` (`file` or `archive`), the `offset` it resumed from, the `bytes`
  sent, the `size`, the `sha256` of what was sent when it completed, and
  `result` (`completed`, `failed` with an `error`, or `cancelled` when the
  browser stopped it).
- `files.upload`, when an upload is checked and in place: `path`, `size`,
  `sha256` and `overwrite`.
- `files.create_folder` (`path`), `files.rename` (`path`, `new_path`),
  `files.move` (`paths` moved, `destination`, the `failed` ones with their
  code) and `files.delete` (`paths` deleted, `trash`, the `failed` ones).
  A move or delete that did nothing isn't recorded.

## Errors

The Agent's refusals reach the website with a code and a sentence that names
the path: `not_found`, `access_denied`, `already_exists`, `not_a_directory`,
`is_a_directory`, `invalid_path`, `invalid_name`, `invalid_move`, `no_user`,
`too_large`, `no_space`, `checksum_mismatch`, `offset_mismatch`, `changed`,
`trash_unavailable`, `busy` and `failed`. A device that is offline is
`device_offline`; one that disconnects while it works on a request,
`device_disconnected`; one that doesn't answer in time (30 seconds, two
minutes for moves, deletes, finishing an upload and preparing a download),
`device_timeout`. The page shows a folder it can't open with what to do:
go up, try again, or switch accounts (access denied as the user, or nobody
signed in). On a Mac, root is still kept out of some folders (Desktop,
Documents, Downloads, other apps' data) unless the MeshRMM Agent has **Full
Disk Access**; the page says so when that's the cause.

## How it works

```text
browser ── HTTPS /v1/agents/{id}/files… ──▶ server ── control connection: file_request ──▶ Agent
        ◀──────────── JSON ──────────────        ◀────────── file_reply ──────────────

browser ── GET  /v1/file-downloads/{id} ──▶ server ── file_transfer ──▶ Agent
        ◀──────── file bytes ────────────        ◀── wss /v1/agents/{id}/file-transfers/{t} ── reads the file
browser ── PUT  /v1/file-uploads/{id} ────▶ server ──────────────────────────────────────────▶ writes the partial file
```

Listings and changes are small, so they go over the Agent's control
connection: the server sends `file_request` (`crates/protocol-types/src/files.rs`)
and waits for the `file_reply` with the same `request_id`. File contents
never take that path. A download or an upload reserves a transfer, the
server sends `file_transfer`, and the Agent connects a WebSocket of its own,
`/v1/agents/{id}/file-transfers/{transfer_id}`, with its credential.

On a transfer socket, binary messages carry the file's bytes, in pieces of
at most 256 KB; text messages are JSON. For a download the Agent sends
`begin` (`size`, unknown for an archive, and `modified`), the bytes, then
`done` (`bytes`, `sha256`), or `failed` with an error. For an upload it
sends `ready` (`received`, which must be where the request starts), takes
the bytes and the server's `end`, writes and syncs them, and sends `done`
(`bytes`: how much the partial file holds).

**Flow control.** TCP paces each leg. The server reads the Agent's next
piece only when hyper asks for more of the download's body, which it does
once the browser has taken the last; the Agent's reading thread waits on a
queue of four pieces, which waits on the socket. An upload's next piece is
read from the browser's request only when the Agent's socket has taken the
last, and the Agent's writer takes from a queue of four. The server holds
one piece at a time either way, so a slow browser or a slow disk slows the
transfer instead of filling memory.

The API, all of it open to API tokens:

| Route | |
|---|---|
| `GET /v1/agents/{id}/files/roots?run_as=` | Drives or `/` and volumes, and users' folders |
| `GET /v1/agents/{id}/files?path=&run_as=&offset=&limit=&sort=&order=&hidden=&filter=&refresh=` | A page of a folder (`limit` 1–1000, 500 by default; `sort` `name`, `size`, `modified` or `type`; `order` `asc` or `desc`) |
| `POST /v1/agents/{id}/files/folders {path, run_as}` | A new folder |
| `POST /v1/agents/{id}/files/rename {path, name, run_as}` | Renames in place |
| `POST /v1/agents/{id}/files/move {paths, destination, run_as}` | `{done, failed}` |
| `POST /v1/agents/{id}/files/delete {paths, trash, run_as}` | `{done, failed}` |
| `POST /v1/agents/{id}/files/downloads {paths, run_as}` | Checks the entries; returns `{id, name, kind, size, files, url, progress}` |
| `GET /v1/file-downloads/{id}` | The bytes; `Range` and `If-Range` resume a file |
| `GET /v1/file-downloads/{id}/progress` | `{state, bytes, offset, size, sha256, error}` of the latest attempt |
| `POST /v1/agents/{id}/files/uploads {path, size, overwrite, run_as}` | Starts an upload: `{id, received}` |
| `GET /v1/file-uploads/{id}` | `{received}`: how much the device has |
| `PUT /v1/file-uploads/{id}?offset=` | Bytes from `offset`, the whole rest or a piece |
| `POST /v1/file-uploads/{id}/finish {sha256}` | Checks and places the file |
| `DELETE /v1/file-uploads/{id}` | Cancels it and removes the partial file |

`run_as` is `system` (the default) or `user`. With curl:

```sh
H="Authorization: Bearer $TOKEN"
curl -sH "$H" "https://rmm.example.com/v1/agents/$DEVICE/files?path=/var/log"
id=$(curl -sH "$H" -H 'Content-Type: application/json' \
  -d '{"paths":["/var/log/syslog"]}' \
  "https://rmm.example.com/v1/agents/$DEVICE/files/downloads" | jq -r .id)
curl -sH "$H" -o syslog "https://rmm.example.com/v1/file-downloads/$id"
```

Server: `server/src/files/` (requests, transfers, downloads and uploads, and
the relay), `server/src/api/files.rs`. Agent: `agent/src/remote/files.rs`
and `files/` (`account`, `listing`, `operations`, `uploads`, `transfer`,
`archive`, and the `windows` and `unix` specifics). Website:
`dashboard/features/files/`.

## Tests

- `cargo test -p meshrmm-protocol-types files` covers the wire types.
- `cargo test -p meshrmm-server --test files` runs on SQLite and PostgreSQL:
  listings and changes through a stand-in Agent, who may do what as whom,
  API tokens, downloads streamed, checked, resumed and aborted, archives,
  uploads interrupted and resumed, checksum and disk-full failures, the
  transfer limits and the audit log.
- `cargo test -p meshrmm-agent files` reads, sorts and pages real folders,
  creates, renames, moves and deletes, writes and checks uploads, and builds
  ZIP archives that unpack.
- `npm test` in `dashboard` covers paths on each platform, the listing's
  windowing, folder uploads and the SHA-256 the browser computes.
