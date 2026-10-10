# Windows credential autofill

The macOS and Windows viewers expose **Prompt for credentials** in the toolbar's
credentials (key icon) menu in an installed-service session. It opens a native
Windows dialog on the viewed endpoint. The remote user enters `DOMAIN\user`, `user@domain`, or `.\localuser`
and their Windows password (not a Hello PIN). The dialog explains how the
credentials will be used. Cancel leaves any previous saved credentials intact.

Windows performs one interactive `LogonUserW` validation per submission. Failed
validation never replaces the saved credential, and its error appears in the key
icon's tooltip. There are no
automatic authentication retries. Local/domain logon policies, account restrictions, and domain availability still apply.

After successful validation, the endpoint encrypts the credential using
user-scoped Windows DPAPI under the LocalSystem helper identity. The service
saves only ciphertext to `autofill-credentials.dat` beside `agent.json`
(`%ProgramData%\MeshRMM\Agent`, restricted to SYSTEM and Administrators), and
only the LocalSystem DPAPI key can decrypt it. Plaintext buffers, including BSTR
copies used by UI Automation, are wiped on release. Neither plaintext nor
ciphertext is sent to the viewer/server, placed on a clipboard, or logged.

Saved credentials persist across remote sessions, reconnects, Windows user
switches, background sessions, agent updates, and reboots. There is one saved
credential per endpoint; a new successful prompt replaces it. Only **Forget saved
credentials** (available in any session mode) or uninstalling the agent deletes
it.

The menu always lists **Autofill saved credentials**. It is enabled, and the key
icon shows a blue dot, while credentials are saved and a supported Windows login
or UAC password field is visible. The prompt does not need focus: detection
checks every visible top-level window of `System32\LogonUI.exe` and
`System32\consent.exe` on the session's desktop, foreground window first, so a
UAC prompt left behind another window still qualifies. Each fill requires a click
and rechecks that the prompt window still exists, is shown, and belongs to the
same process. Only visible, enabled, unambiguous password controls belonging to
that process are accepted. Hello PINs, arbitrary application password boxes, consent-only UAC
prompts, and unsupported credential providers do not offer autofill. Detection
runs separately from mouse/keyboard input.

Autofill uses the verified controls' UI Automation Value pattern; it does not
send global keystrokes or assume a tab order. If Windows exposes a username
field it fills both fields. Otherwise select the correct Windows account first.
Review the selected account and submit the form yourself. Unsupported providers
fail without a keystroke fallback. Passwordless/MFA workflows remain interactive.

Session modes that have no credential broker leave the controls disabled.
Nothing about credentials involves the server.

## Tests

`cargo test -p meshrmm-agent credentials` covers which prompts and fields are
accepted, saved credentials lasting until they are forgotten, and DPAPI
round trips with tamper rejection. The DPAPI test fails with "Access is
denied" in a key-based SSH session, which has no DPAPI master key; run it
from a logged-in desktop. To try a password prompt behind another window, set
UAC to ask for credentials (`ConsentPromptBehaviorAdmin` = 3) for the test
and put it back afterwards.
