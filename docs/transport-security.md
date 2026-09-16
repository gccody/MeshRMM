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
- Clean disconnect left the Windows service running. These initial live sessions
  used direct ICE; forced TURN was subsequently verified below.

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

## Forced TURN validation — September 16, 2026

Temporary Windows firewall rules blocked only the Agent executable's direct UDP
paths to the Mac's LAN, VPN, and public addresses. A fresh installed-service
session selected a Cloudflare TURN relay and negotiated
`Tls_Ecdhe_Ecdsa_With_Aes_128_Gcm_Sha256`. Screen presentation, remote mouse and
keyboard input, and chat in both directions passed over this relayed connection.

The first attempt discovered the additional VPN path. Blocking that already-live
path interrupted teardown, and immediate retries received HTTP 409 until the
test session was explicitly closed using the dashboard's Close session action.
The subsequent fresh session completed the forced-relay check successfully.

The test rules had automatic cleanup and were also explicitly removed and
verified absent. The viewer was disconnected and the Windows service remained
running and online. No production code changed for this additional validation.

## Strict public TLS follow-up — September 16, 2026

The zone minimum is now **TLS 1.3**, superseding the TLS 1.2 minimum above.
This removes CBC and static-RSA key exchange without purchasing ACM. TLS
1.2-only clients are intentionally no longer supported on these public hosts.
The installed Windows Agent uses rustls and reconnected after a service restart;
the authenticated Chrome dashboard reloaded and the installed macOS viewer
redeemed a new handoff and rendered the remote desktop successfully.

The checker now defaults to requiring TLS 1.3, explicitly tests eight CBC and
static-RSA alternatives, and requires forward secrecy as well as AEAD when
checking an optional `--minimum-tls 1.2` policy. Its negative probes fail on
inconclusive network, certificate, or local cipher errors. Six unit tests cover
these outcomes and run in CI. Before the setting changed the new check exposed
all eight weak alternatives; afterward all 78 probes across the six public
hostnames passed. The probes cover the listed alternatives, not every possible
cipher implementation.

## Acknowledged session cleanup — September 16, 2026

The viewer now follows an intentional disconnect with an HTTPS `POST` to the
session's `/end` endpoint, authenticated with its session token. The operation
is idempotent and retries transient failures three times with bounded timeouts.
It works independently of the session WebSocket. Unauthorized tokens, including
an Agent token, cannot release the lease. The server revokes the session before
cleanup and retains a retry alarm until the coordinator acknowledges lease
release, instead of discarding the only cleanup record on a failed request.

Validation: macOS viewer Clippy/tests, server native tests, WASM check/build,
SQL regressions, and Windows workspace Clippy/tests passed. A local workerd
integration test on macOS/Node 22.23.2 verified close without a signaling socket,
wrong-token rejection, repeated close, token revocation, fresh-session lease
acquisition, and protection against cleanup from an older session. No Agent
binary change or service reinstall was required for this feature.

Deploy the server endpoint before distributing this viewer change. The new
server has not been deployed to production; the production fault-injection
retest therefore remains a release validation step. Full network loss can still
prevent a close request from reaching the server; the existing idle lease
provides eventual expiry in that case. A failed acknowledgment is reported. An older server returning HTTP 404 retains
the legacy WebSocket close behavior with a warning, so rolling upgrades do not
turn an otherwise successful disconnect into an application error. This fallback
does not claim acknowledged cleanup; deploy the server first to obtain it.

## Independent peer enrollment

Both native endpoints now use persistent local DTLS certificates and require an
administrator-enrolled SHA-256 fingerprint for the other peer. Every advertised
SDP fingerprint must match the same enrolled identity, and WebRTC must then
verify that the actual DTLS peer possesses its private key. Unknown, malformed,
conflicting, missing, changed, or revoked identities fail closed. There is no
trust-on-first-use, dashboard enrollment, or compatibility bypass for old peers
that generate a new certificate on every session.

The trust store is an allowlist of permitted peer identities. It does not bind a
browser's displayed device name to a fingerprint; a compromised dashboard can
still mislabel or redirect a request to another already-trusted device. Only
enroll endpoints that should be permitted to connect. This protects remote
channel peer authentication; it does not remove control-plane authority over
other Agent administration operations or make a compromised endpoint safe.

Before distributing the updated native binaries, provision both identities and
exchange their public fingerprints through a separately authenticated channel
(for example, an administrator's established SSH connection or physical access).
Do not copy private identity files between endpoints or obtain the fingerprint
solely from the signaling error or dashboard.

Run the appropriate binary locally (an elevated shell for the Agent):

```text
meshrmm-agent.exe --identity-fingerprint
meshrmm-remote --identity-fingerprint

meshrmm-agent.exe --trust-peer <verified-viewer-SHA256-fingerprint>
meshrmm-remote --trust-peer <verified-agent-SHA256-fingerprint>
```

On macOS, the installed executable is
`~/Applications/MeshRMM Remote.app/Contents/MacOS/meshrmm-remote`. The commands
operate without a handoff token or server connection. Fingerprints accept
colon-separated or contiguous hexadecimal digits. Existing trust entries are
never overwritten implicitly. Use `--revoke-peer <fingerprint>` to remove a
pin; close active sessions to apply a revocation immediately. A new identity
requires independent verification and explicit enrollment again.

The Agent stores its key and pins under `%ProgramData%\MeshRMM\Agent\identity`,
inheriting the installer's SYSTEM/Administrators-only ACL. The viewer uses
`%APPDATA%\MeshRMM\viewer-identity` on Windows and
`~/Library/Application Support/MeshRMM/viewer-identity` on macOS. Unix directories
are owner-only and private-key files are mode 0600; Windows viewer files inherit
the current user's profile permissions. Corrupt or expired identity files are
not replaced automatically. Identities expire after five years and require
administrator rotation. Protect these directories in backups and keep them
outside release artifacts. `--identity-directory <path>` is available on the
administrative commands for provisioning/testing; normal sessions use the
standard locations above.

Release order: deploy the acknowledged-close server first; provision local
identities and mutual pins; then distribute both native endpoint updates.
Unenrolled viewers and older ephemeral-certificate peers will be blocked by
updated endpoints. Do not enable fleet auto-update until enrollment is complete.

### Identity validation — September 16, 2026

- Six new identity tests cover persisted certificates, corrupt/expired key
  rejection, explicit enrollment and revocation, malformed/conflicting SDP,
  mutually pinned real WebRTC payload exchange, and a real attacker presenting
  a copied trusted fingerprint without its private key. The latter reached a
  failed DTLS connection, rather than merely timing out.
- macOS viewer/session-transport Clippy and tests passed. Windows workspace
  Clippy, tests, and release build passed against hash-verified source. CI now
  exercises the shared transport security tests on macOS as well as Windows.
- An installed viewer rejected an unenrolled peer and displayed the verification
  error without opening remote channels. Both test endpoint fingerprints were
  subsequently obtained through local execution and authenticated SSH and
  enrolled explicitly. The Windows private-key ACL was verified as SYSTEM and
  Administrators only.
- The final installed Agent SHA-256 is
  `896ca944f4d3b9da3d5c5066873774809e2113dd703a32140c6ecb11cd272ca4`.
  The supported installer preserved configuration and confirmed reconnection.
  The identity persisted across the subsequent build installation and restart.
- Installed endpoints passed screen/input, bidirectional chat, a 311,296-byte
  file delivery verified by the receiver, and generated audio delivery. Forced
  Cloudflare TURN passed screen/input, bidirectional chat and clipboard, with
  the expected fingerprint visible in diagnostics and ECDHE-ECDSA AES-GCM in
  both endpoint logs. Temporary firewall rules were removed and the service
  was left online.
- The final viewer disconnected cleanly against the existing production server
  using the explicitly logged legacy-close fallback. The new acknowledged-close
  endpoint passed local workerd integration tests; its production fault-recovery
  check is still required after the server deployment and before fleet rollout.
