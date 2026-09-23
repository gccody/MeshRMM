# Agent tray icon

Replace `tray.ico` and rebuild the agent to change the icon. It is embedded in the
executable; there is no installer or asset-copy step. Use a single-image, 32-bit
RGBA Windows ICO (32×32 recommended). The current blue/white M is a temporary
placeholder. Keep transparency so the icon works on light and dark taskbars.

The passive Windows tray helper runs as each active signed-in user. Hovering shows
“MeshRMM Agent is running”; clicks have no action or menu. Windows may initially
put the icon in the notification-area overflow. This indicates the installed agent
service is running, not that the server is reachable. The service owns the helper's
lifetime, restarts it after failure, and removes it on stop; Explorer restarts are
handled by re-registering the icon. No configuration or credentials enter the helper.
