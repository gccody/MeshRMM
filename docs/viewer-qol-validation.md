# Viewer quality-of-life validation

## Agent display border

The viewer's session controls include **Highlight viewed monitor on agent**.
The setting starts enabled unless the company disables its default. Session
choices survive transport reconnects. The agent draws a three-pixel red outline
on the selected physical monitor, or on each monitor in **All monitors** mode.
The outline is excluded from captured video, cannot take focus or intercept
input, and is destroyed when capture ends. Company defaults apply to new sessions.

Deploy migration `0009_display_border.sql` with the server/dashboard update.
No production deployment is part of this change.

Validation on September 16, 2026:

- macOS: viewer Clippy and unit tests; protocol and server tests; server WASM
  check; SQL regressions; dashboard `npm run verify` using Node 22.22.0.
  Server/dashboard CI normally uses Linux.
- Windows `DESKTOP-85R6S28`: source hashes checked against the local working tree;
  workspace Clippy and tests; release agent build. The native overlay test runs
  separately on the interactive desktop because SSH has no composited desktop.
- Native overlay assertions cover geometry, capture exclusion, input transparency,
  and destruction on drop. Protocol tests preserve existing wire tags; company
  tests cover SQLite boolean conversion and legacy defaults.
- Installed-service testing: viewer toggle removes/restores the overlay; changing
  from the primary landscape monitor to the left portrait monitor moves it to
  the correct negative coordinates; closing the viewer removes every border
  window. The viewer's captured video contains no red outline.

The Windows cross-check attempted on macOS lacked MSVC C headers; native Windows
validation was used. An initial SSH-only overlay test could not create composited
windows; the same test passed in the logged-in Windows desktop.

Final installed border-build SHA-256:
`8B521281D7DCAFFD3359A547BB05A5FF22ACF2F25729D8A3FE59109DADA9825D`.
The all-monitor service test produced eight excluded border windows (four per
physical monitor), with no outline visible in the combined captured video.
