# Remote transport security

MeshRMM's authenticated API and signaling clients require HTTPS and WSS.
The shared URL builder rejects HTTP/WS, and the authenticated WebSocket
connector also rejects non-WSS URLs before opening a connection or attaching
credentials. Viewer session HTTP requests and Agent enrollment are HTTPS-only,
including redirects. Local plaintext WebSockets are used only by isolated
network-liveness unit tests, without the authenticated connector.

Screen, audio, input, clipboard, files, and chat use WebRTC data channels.
TURN relays forward the DTLS-encrypted traffic. Peer certificate fingerprints
are authenticated through the signaling service; the control plane remains
trusted for peer identity. These controls do not provide independent device
identity verification against a compromised signaling service or establish
FIPS certification.

## Secure URL validation — September 16, 2026

- Regression tests cover plaintext rejection before a network connection,
  secure URL construction, path encoding, and query/fragment replacement.
- macOS: signaling-client and viewer tests, Clippy for both packages, and
  workspace formatting passed.
- Windows: source files were SHA-256 verified in a dedicated validation
  directory; workspace Clippy and tests passed. Existing hardware/interactive
  tests remain ignored by the normal suite.
- The locally installed macOS viewer connected to the existing Windows service
  through the dashboard handoff. Screen rendering, remote mouse/keyboard input,
  bidirectional chat, and clean disconnect worked.
- Installed the updated Windows Agent through `install-agent-local.ps1`; the
  installer verified the executable hash, preserved configuration, and confirmed
  service startup and signaling reconnection. A fresh dashboard session rendered
  the desktop successfully with both updated endpoints. Agent SHA-256:
  `0c5d760ee30b1ce431839139a364e109383c1613fa4a83160eb13a8bd479b419`.

Detailed local command output is retained under `dist/security-validation/`.
