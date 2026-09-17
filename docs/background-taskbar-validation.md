# Background taskbar validation

Validated September 16, 2026 on Windows endpoint `DESKTOP-85R6S28`.

The floating launcher is now a bottom, topmost taskbar with native application
icons and labels for ten administrative tools. Task Manager is pinned alongside
Resource Monitor; its ability to start in Session 0 remains unverified. Capture
now requests full window content, falling back to the existing PrintWindow path
when that request fails. This does not provide an interactive-user shell or
promise compatibility with all GPU-rendered applications.

Checks against the synchronized working tree:

- Windows: `cargo clippy -p meshrmm-agent -p meshrmm-remote-screen --all-targets -- -D warnings` passed.
- Windows: `cargo test -p meshrmm-agent -p meshrmm-remote-screen` passed: 65 tests, 19 ignored.
- Windows: release Agent build passed.
- Windows: the ignored `remote::background::tests::session_zero_gui` test passed
  in a dedicated SYSTEM Session 0 process. It exercised clicking Registry Editor
  on the taskbar, window rendering, H.264 encoding, mixed-case PowerShell keyboard
  input, and application cleanup. The captured bitmap was visually inspected:
  the taskbar spans the bottom edge with all ten icons and labels visible.
- macOS: `cargo fmt --all -- --check` and `git diff --check` passed.

The supported local installer installed and connected build SHA-256
`AF9C0115D1031D996232D10D47182EA08BA127C79B58720CA819166CC7AEC83C`, preserving
configuration. The automatic updater immediately replaced it because this
checkout's release metadata says 0.2.5 while the published release is 0.2.7.
A viewer connection therefore exercised the replacement release, not the changed
background implementation. Installed-service end-to-end validation is incomplete;
the successful isolated Session 0 test does not substitute for it. The updater
and configuration were left unchanged, and the endpoint was left running the
working published service. No release was published or pushed.
