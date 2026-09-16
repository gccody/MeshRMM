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

The pinned DTLS defaults allow only ECDHE with AES-128-GCM-SHA256 or
ChaCha20-Poly1305-SHA256. CBC-only peers cannot establish a session. Existing
MeshRMM clients already support AES-GCM, so mixed-version sessions remain
compatible. The small dependency patch and upgrade requirements are documented
in [the vendor note](../vendor/dtls/MESHRMM.md). Each completed handshake logs
the negotiated DTLS cipher; it does not log keys, tokens, or session payloads.

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

## DTLS policy validation — September 16, 2026

- Real DTLS handshakes test GCM defaults, ChaCha20-only peers, older peers
  preferring CBC but supporting GCM, and CBC-only rejection in both roles.
  Successful handshakes verify the negotiated cipher and bidirectional payloads
  with certificate verification enabled.
- macOS viewer and session-transport tests and Clippy passed, including the
  existing WebRTC test that stalls file consumption while input remains usable.
- Windows workspace Clippy and tests passed against hash-verified source.
- The updated macOS viewer connected to the previous Agent build, rendered the
  screen, and delivered chat. The live handshake logged
  `Tls_Ecdhe_Ecdsa_With_Aes_128_Gcm_Sha256`.
- Installed the patched Agent using the supported local installer. SHA-256:
  `b4c420c30774598ab513a07bdf0112a31d40b426d3680fe637e6756a2509bc62`.
  Both endpoint logs confirmed AES-GCM negotiation. Configuration was preserved.
- With both updated endpoints: screen rendering, all-monitor selection and
  restoration, remote mouse/keyboard input, chat, and clipboard in both directions
  passed. A 311,296-byte generated file transferred to Windows and back with
  SHA-256 `aa43c7704a936a47a9d3dda43ce6203ecd8176a341786fdc9c1732f09a630b51`
  matching on both machines. A generated five-second tone produced audio packets
  and initialized macOS playback at 48 kHz stereo. This verifies the audio path,
  not subjective sound quality.
- Clean disconnect left the Windows service running. These live sessions used
  direct ICE; a forced TURN session was not separately exercised.

## Public TLS policy — September 16, 2026

The `meshrmm.com` Cloudflare zone uses these Edge Certificates settings:

| Setting | Value |
| --- | --- |
| Minimum TLS Version | 1.2 |
| TLS 1.3 | Enabled |
| Always Use HTTPS | Enabled |

These zone settings were applied directly; no Worker or native release was
published. Before the change, live probes accepted TLS 1.0 and 1.1. Afterward,
all six production hostnames rejected both with a protocol-version alert,
accepted TLS 1.2/1.3 with valid certificates and AEAD ciphers, and redirected HTTP
to HTTPS. The Windows service reconnected after restart, the authenticated
dashboard reloaded, and a fresh remote handoff established an AES-GCM session.

Repeat the read-only checks from a Python 3/OpenSSL host:

```sh
python3 scripts/check-transport-security.py \
  meshrmm.com www.meshrmm.com admin.meshrmm.com auth.meshrmm.com \
  api.meshrmm.com internal.meshrmm.com
```

The checker exits nonzero for accepted legacy TLS, invalid certificates,
non-AEAD negotiated modern ciphers, missing HTTPS redirects, or inconclusive
probes. An invalid-DNS negative check also verified failure rather than a false
pass. This tests negotiated ciphers, not every cipher the edge might accept.
Cloudflare's full TLS 1.2 cipher allowlist remains provider-managed; custom
allowlists require Advanced Certificate Manager, which was not purchased.
