# Agent tray icon

Replace `tray.ico` and rebuild the agent to change the icon. It is embedded in the
executable; there is no installer or asset-copy step. Use a single-image, 32-bit
RGBA Windows ICO (32×32 recommended). The current blue/white M is a temporary
placeholder. Keep transparency so the icon works on light and dark taskbars.

The Windows tray helper runs as each active signed-in user. Hovering shows
“MeshRMM Agent is running”, and outside a remote session clicks do nothing.
During a session the tooltip offers the chat and a click opens it. Windows may
initially put the icon in the notification-area overflow. The icon shows that
the installed agent service is running, not that the server is reachable. The
service owns the helper's lifetime, restarts it after failure, and removes it on
stop; Explorer restarts are handled by re-registering the icon. No configuration
or credentials enter the helper.

# Virtual display driver

`sudovda/` holds SudoVDA 1.10.9.289, the signed Indirect Display Driver the
Agent installs on computers without a monitor (see
[video](../../docs/video.md#computers-without-a-monitor)). The
files are copied unchanged from Apollo's release package, whose catalog is
signed by `CN=sudovda@su.mk`; changing any of them breaks the signature. The
Agent embeds them, so there is no separate copy step.
