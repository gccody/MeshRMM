# Viewer user sessions and displays

The macOS and Windows viewer toolbars have separate user-session and display
selectors. Console lists its monitors, Background has one disabled Display 1
selector, and each active RDP session lists its own monitors as Display 1, 2, 3,
etc. The existing combined-monitor entry remains available as All displays.
Monitor keyboard shortcuts stay within the selected session. A session change
selects that session's primary monitor, and the controls commit the selection
when the agent confirms the replacement stream.

The Windows service discovers active non-console users through WTS and enumerates
monitors by launching a helper in each user's Windows session. Capture, input,
clipboard, file transfer, and chat helpers move together when the selected user
changes. RDP display IDs include the Windows session identity; the coordinator
maps them back to native monitor IDs for input, file destinations, and pointer
indicators. A failed switch restores the previous session when it is available.
The service checks for session arrivals/departures every five seconds and republishes
the catalog through the existing stream-reconfiguration path.

Session metadata changes the display wire format, so the control channel is now
v5. Install matching agent and viewer builds together; v4 viewers are incompatible.
No server or dashboard schema change is needed. Console-only service audio and
Ctrl+Alt+Del are unavailable while viewing RDP or Background.

## Validation

Validation uses macOS for the native macOS viewer and DESKTOP-85R6S28 for native
Windows agent/viewer checks and the installed service. Source is copied into a
dedicated directory and SHA-256 checked against the working tree before checks.

Automated cases cover multiple RDP users (including identical usernames in
different Windows sessions), three RDP monitors, separate console/background
lists, session metadata round trips, noncolliding monitor IDs, pointer mapping,
helper token selection, and return-to-console route cleanup.

Live multi-monitor RDP validation is incomplete: no RDP client/session was
available, and the user confirmed they cannot provide one for this run.

Validated on September 23, 2026:

- macOS: protocol tests (34), viewer tests (32 passed, one existing ignored),
  Clippy with warnings denied, and a locally installed release viewer.
- Windows: agent tests (81 passed, 15 existing ignored), service environment test
  (one passed), protocol tests (34), viewer tests (24 passed, one existing ignored),
  Clippy with warnings denied, and a release agent build. Native checks used the
  pinned Rust 1.97.1 toolchain. Windows viewer UI was not manually exercised.
- The supported local installer preserved configuration, backed up the old agent,
  started the updated service, and verified signaling reconnection.
- The macOS viewer connected to that installed service. Console offered Display 1,
  Display 2, and All displays. Both individual monitors and the combined stream
  worked. Background rendered its workspace and exposed only disabled Display 1;
  the next-display shortcut stayed in Background. Returning to Console restored
  both physical choices and selected its primary monitor.
- Service logs confirmed all switches (332–633 ms) without capture/helper errors.
  ICE emitted nonfatal address-gathering warnings; the live connection succeeded.
  The viewer disconnected cleanly, and MeshRMMAgent remained Running.

Installed agent SHA-256:
`B58CA41225291D3E2267AF766BF9E7B37125D42A2F22593A9DD980C07E5AD9D0`.

Installed macOS viewer SHA-256:
`48c814f5eb884dfafda7ec10d10be219b2cd727be852d3ab745ccd2f0a3134c1`.

Validation logs are retained locally in `/tmp/meshrmm-sessions-validation`.
No server/dashboard checks or production release were performed.
