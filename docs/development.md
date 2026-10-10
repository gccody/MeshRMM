# Development

Building MeshRMM from source, running it locally, and checking a change.
MeshRMM is in heavy development with no deployments to protect: change wire
protocols, database schemas and configuration formats freely, and don't add
migrations or compatibility paths for old versions (see `AGENTS.md`).

## Prerequisites

- **rustup.** The first `cargo` command in the repository installs the
  toolchain pinned in `rust-toolchain.toml`, with Clippy and rustfmt. A
  `cargo` that rustup doesn't manage, such as Homebrew's, ignores that file;
  put rustup's first in `PATH`.
- **CMake and a C compiler**, which build the bundled libopus audio codec. On
  Windows the Visual Studio C++ build tools provide the compiler, and their
  "C++ CMake tools for Windows" component provides CMake.
  `scripts\use-cmake.ps1`, which the build scripts dot-source, finds it with
  `vswhere` when `cmake` isn't on `PATH`; dot-source it yourself before
  running `cargo` directly.
- **Node.js 22.13 or newer**, for the website and the release scripts.
- For the Agent and the Windows viewer: Windows 10 version 1903 or newer.
  For video at full speed, a GPU and driver with Media Foundation hardware
  H.264 encode and decode and D3D11 NV12 video processing; without one, video
  falls back to software H.264. H.265 and 4:4:4 are used when both ends have
  them.
- For the macOS viewer: macOS 12 or newer. For the macOS Agent: 12.3 or
  newer.
- For the server: Linux to run a release, though it builds and runs on macOS
  for development. PostgreSQL is optional.

## What builds where

| Target | Windows | macOS | Linux |
|---|---|---|---|
| `meshrmm-agent` | yes | yes | no |
| `meshrmm-remote` (viewer) | yes | yes | no |
| `meshrmm-server` | not supported | development only | yes |
| `dashboard/` | yes | yes | yes |

`cargo build` and `cargo test` without `-p` cover the Agent and the viewer
only. Name the server: `cargo build -p meshrmm-server`.

## Run a server locally

Build the website, then run the server in proxy mode with a throwaway data
directory. [The website's README](../dashboard/README.md#local-development)
has the `server.toml`, and the Vite dev server that proxies to it and
presents the server's origin so the same-origin checks pass.

```sh
cd dashboard && npm ci && npm run build && cd ..
cargo run -p meshrmm-server -- -c server.toml   # logs the first-run setup link
```

A debug server reads `dashboard/dist` when it starts; a release build embeds
it. A server built before the website has no pages, and says so at startup.

To serve local Agent and viewer builds, set `downloads.dir` to
`dist/downloads` (see below). Agents and viewers accept only HTTPS with TLS
1.3 and a certificate the operating system trusts, so a device needs a real
name and certificate for the server, or a development CA it trusts.

## Build the Agent and the viewer

These scripts read the version from `release.json`, put the build in
`dist/downloads/`, and describe it in `dist/downloads/artifacts.json`:

```powershell
& .\scripts\build-agent.ps1      # also dist\agent\
& .\scripts\build-remote.ps1     # also dist\remote\
```

```sh
sh scripts/build-agent-macos.sh    # universal; also dist/agent-macos/
sh scripts/build-remote-macos.sh   # this Mac's architecture; also dist/remote-macos/
```

The builds are unsigned unless `MESHRMM_RELEASE_SIGNING_KEY` holds a key. A
server serves unsigned builds to install, but installed Agents and viewers
won't update to them. To test updates end to end, make a development key;
see [releases](releases.md#local-and-development-builds).

### Windows

To try Agent changes on a computer that is already enrolled, without
publishing anything:

```powershell
& .\scripts\install-agent-local.ps1
```

It builds the checkout, asks for UAC, keeps the installed configuration,
replaces the service's executable, and checks the installed hash and that
signaling reconnects. If installation or startup fails it restores the
timestamped backup. Restarting the service interrupts a live session.
`-SkipBuild` installs an existing `target\release\meshrmm-agent.exe`. Results
and a copy of the log go under `dist\`. Automatic updates stay on, so a newer
release on the device's server can replace the local build.

`meshrmm-agent.exe --console --config agent.json` runs the Agent in the
current session instead of as a service. Ctrl+Alt+Del, desktop switches and
background mode need the installed service.

For the viewer, copy `dist\remote\` anywhere and open `meshrmm-remote.exe`
once to register the `meshrmm` link.

### macOS

```sh
sh scripts/install-remote-macos.sh
```

builds and signs a release-mode viewer, closes a running one, installs it in
`~/Applications/MeshRMM Remote.app` and registers website links. It writes a
`remote.json` with `auto_update` set to `false`, so a release can't replace
the local build; reinstall a release to get updates back. An existing install
is kept in a `.meshrmm-backup.*` folder under `~/Applications`.
`sh scripts/build-remote-macos.sh --local` makes the same bundle without
installing it.

The macOS build scripts sign with the keychain's Developer ID Application
certificate, which keeps the Agent's privacy permissions across rebuilds. If
the keychain holds several, set `MESHRMM_CODESIGN_IDENTITY` to the one to
use. Set it to `-` for an ad-hoc signature, as the release workflow does;
then the permissions reset on every build, and an installed Agent signed by
a team refuses to update to it. Locally built apps aren't quarantined, so
they need no notarization.

## Checks

CI (`.github/workflows/ci.yml`) runs each of these only when a path it
depends on changed. Run the ones your change touches.

On Windows, for the Agent, the viewer and the shared crates:

```powershell
cargo fmt --all -- --check
cargo clippy --locked --workspace --exclude meshrmm-server --all-targets -- -D warnings
cargo test --locked --workspace --exclude meshrmm-server
```

On macOS, for the viewer, the macOS Agent and the shared transport:

```sh
cargo clippy --locked -p meshrmm-remote -p meshrmm-agent -p meshrmm-session-transport --all-targets -- -D warnings
cargo test --locked -p meshrmm-remote -p meshrmm-agent -p meshrmm-session-transport
```

For the server:

```sh
cargo clippy --locked -p meshrmm-server --all-targets -- -D warnings
cargo test --locked -p meshrmm-server
python3 -m unittest discover -s scripts/tests -v
node --test scripts/*.test.mjs
```

The server's integration tests start it in-process and drive its HTTP and
WebSocket APIs with fake Agents and viewers. They run on SQLite, and also on
PostgreSQL when `MESHRMM_TEST_POSTGRES_URL` names a server where they may
create databases, for example
`postgres://postgres:postgres@localhost:5432/postgres`. CI runs both.
`tests/schema_parity.rs` checks that the two backends' migrations make the
same tables and columns; change both `server/migrations/sqlite` and
`server/migrations/postgres` together.

For the website and the marketing page:

```sh
cd dashboard && npm ci && npm run verify   # typecheck, lint, build, tests
node --test site/*.test.mjs
```

Windows-only code doesn't compile on a Mac, so macOS Clippy says nothing
about it. `cargo xwin clippy --target x86_64-pc-windows-msvc --workspace
--exclude meshrmm-server --all-targets -- -D warnings` type-checks it from a
Mac (it needs `cargo-xwin` and `ninja`); the tests still need Windows.

Packaging has its own CI job, which builds the static server, packs it with
stand-ins for the Agent and viewer builds, and installs and runs both the
tarball and the Docker image. `scripts/test-server-package.sh` changes the
machine it runs on, so leave it to CI.

## Hardware and desktop tests

Tests that need a GPU, a real desktop or Session 0 are `#[ignore]`d. Run them
by name when you change what they cover. Run them from the logged-in Windows
desktop: an SSH session alone has no composited desktop to capture.

Capture and encoding (`meshrmm-remote-screen`):

```powershell
.\scripts\benchmark-capture.ps1   # animated window, 10 s, prints FPS and latency; needs 30 FPS
cargo test -p meshrmm-remote-screen --release hardware_encode_1440p60 -- --ignored --nocapture
cargo test -p meshrmm-remote-screen all_monitors_stream_and_switch_back -- --ignored --nocapture
cargo test -p meshrmm-remote-screen every_display_produces_keyframes -- --ignored --nocapture
cargo test --release -p meshrmm-remote-screen software -- --ignored --nocapture
```

`hardware_encode_1440p60` isolates conversion and encoding from capture.
`all_monitors_stream_and_switch_back` needs two monitors, and
`every_display_produces_keyframes` a portrait one. The last line is the
software H.264 encoder.

Input blocking, blackout and the wallpaper and window-drag settings, which
briefly block input, cover the monitors and change desktop settings:

```powershell
cargo test -p meshrmm-agent live_ -- --ignored --nocapture --test-threads=1
```

Recording, checked against FFmpeg (the fixtures are described in
`remote/tests/fixtures/README.md`):

```sh
FFMPEG=/path/to/ffmpeg cargo test -p meshrmm-remote ffmpeg_decodes -- --ignored
```

`scripts/test-helper-isolation.ps1` suspends one helper of an installed Agent
for a while, to check that the rest of a live session keeps working; see
[architecture](architecture.md#isolation-rules).

### Session 0 tests

The [background mode](background-mode.md) tests are in
`agent/src/remote/background/tests`, `agent/src/remote/background/input_tests`
and `agent/windows/remote-screen/src/background.rs`. Each must run in its own
SYSTEM process in Session 0, with no background workspace open, for example
from a scheduled task that runs the test executable as SYSTEM. They write
their screenshots as `.bmp` files to that process's temporary directory.

- `remote::background::tests::session_zero_gui`: the launcher, text and
  PowerShell keyboard input, Registry Editor rendering, H.264 output, and
  cleanup.
- `task_manager_opens_in_background` and `file_browser_opens_in_background`:
  the built-in tools.
- `stable_taskbar_and_management_caption` and
  `retained_windows_survive_refresh_budget_and_close`: capture stability.
- `remote::background::input_tests`: menus, right-click menus,
  double-clicks, scrollbars and the wheel, on test windows, Registry Editor,
  Services and Disk Management.

## Releasing

Increase `version` in `release.json` and squash-merge into `main`. See
[releases](releases.md).
