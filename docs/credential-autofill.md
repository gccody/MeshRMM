# Windows credential autofill

The macOS and Windows viewers expose **Prompt for credentials** at the top of
an installed-service session. It opens a native Windows dialog on the viewed
endpoint. The remote user enters `DOMAIN\user`, `user@domain`, or `.\localuser`
and their Windows password (not a Hello PIN). The dialog explains how the
credentials will be used. Cancel leaves any previous saved credentials intact.

Windows performs one interactive `LogonUserW` validation per submission. Failed
validation never replaces the saved credential, and its error appears in the
viewer status. There are no automatic authentication retries. Local/domain logon
policies, account restrictions, and domain availability still apply.

After successful validation, the endpoint encrypts the credential using
user-scoped Windows DPAPI under the LocalSystem helper identity. The service
saves only ciphertext to `autofill-credentials.dat` beside `agent.json`
(`%ProgramData%\MeshRMM\Agent`, restricted to SYSTEM and Administrators), and
only the LocalSystem DPAPI key can decrypt it. Plaintext buffers, including BSTR
copies used by UI Automation, are wiped on release. Neither plaintext nor
ciphertext is sent to the viewer/server, placed on a clipboard, or logged.

Saved credentials persist across remote sessions, reconnects, Windows user
switches, background sessions, agent updates, and reboots. There is one saved
credential per endpoint; a new successful prompt replaces it. Only **Forget
credentials** (available in any session mode) or uninstalling the agent deletes
it.

When a supported Windows login or UAC password field is visible, **Autofill
credentials?** appears. Each fill requires a click and rechecks the foreground
process and fields. Only `System32\LogonUI.exe` and `System32\consent.exe` are
accepted, with visible, enabled, unambiguous password controls belonging to that
process. Hello PINs, arbitrary application password boxes, consent-only UAC
prompts, and unsupported credential providers do not offer autofill. Detection
runs separately from mouse/keyboard input.

Autofill uses the verified controls' UI Automation Value pattern; it does not
send global keystrokes or assume a tab order. If Windows exposes a username
field it fills both fields. Otherwise select the correct Windows account first.
Review the selected account and submit the form yourself. Unsupported providers
fail without a keystroke fallback. Passwordless/MFA workflows remain interactive.

Older agents and foreground/background modes that do not support the credential
broker leave the controls disabled. No server/dashboard deployment is required.

## Validation — September 23, 2026

- macOS: viewer Clippy with warnings denied, 32 viewer tests (one existing
  ignored), 37 protocol tests, release build and local app installation.
- Windows `DESKTOP-85R6S28`: dedicated source directory synchronized with all
  working-tree source/new files and SHA-256 verified; Rust 1.97.1 tools checked.
  Native Clippy with warnings denied; 87 agent tests (15 existing ignored),
  one service-environment test, 37 protocol tests, and 24 Windows viewer tests
  (one existing ignored) passed. DPAPI tampering, trusted process/field selection,
  bounded helper IPC, and appended protocol tags have regression coverage.
- Supported local service installation preserved configuration, retained a backup,
  started the service, and verified signaling reconnection. The first connection
  timed out before the agent received the request; restarting the service's
  signaling connection recovered the live session.
- A macOS viewer connected to the installed Windows service. Remote prompting,
  successful Windows validation, masked lock-screen fill and successful unlock,
  masked UAC password fill, button appearance/disappearance, cancellation,
  failed validation without replacing the saved credential, and explicit forget
  were exercised. UAC was cancelled after fill; no application was elevated.
  The temporarily strengthened UAC credential-prompt policy was restored to its
  original value (admin=5, user=3, secure-desktop=0).
- Passwords were entered through the native Windows dialog. Failed-validation
  testing used a nonexistent account to avoid failed attempts against the real
  user. No test account was created.
- Desktop transitions produced capture-access-loss/retry and clipboard-access
  warnings; capture and input recovered, and the installed service remained
  running. Windows viewer UI and third-party credential providers were not
  manually tested. No server/dashboard changes, checks, or release publishing.

Installed Windows agent SHA-256:
`1A7EC58FD9047C5DA27DD4125B6FF3A1284FF109E1B36E5BE753923A4F160615`.

Installed macOS viewer SHA-256:
`86555a65dcaeae2e5cc0277995a12538c700dbe047c99dd327647a413f55608f`.

Builds use working-tree changes on top of
`93aed17f5873697c521a573e83764dd90145a960`. Local validation logs are retained at
`/tmp/mesh-credentials-*.log`; endpoint sources and installer results are under
`C:\Users\gccody\credentials-validation-20260923`.
