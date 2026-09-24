# Remote transport security

## Current peer acceptance policy

Authenticated sessions automatically accept peer certificate fingerprints supplied
by signaling. No administrator verification, fingerprint prompts, or local
pre-enrollment is required on either endpoint, including new viewers and replaced
peer keys. Persistent local certificates, SHA-256 SDP validation, DTLS certificate
matching/private-key proof, ECDHE/AEAD encryption, and TLS 1.3 remain enforced.
Peer identity trusts the authenticated signaling service; this is not independent
identity verification against a compromised control plane or trust-on-first-use
key-change detection. The earlier manual enrollment requirement has been removed. Existing `trusted-peers` entries are not used for admission.

`--revoke-peer <fingerprint>` optionally blocks that exact certificate locally;
`--trust-peer <fingerprint>` removes such a block. Normal connections need neither
command. Close an active session when applying a local block. Certificate blocking
does not revoke an account or device's server authorization; revoke that access
through the normal administration controls when appropriate.


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

## Persistent endpoint certificates

Local certificates survive restarts and upgrades. The Agent uses
`%ProgramData%\MeshRMM\Agent\identity`, protected by the installer's
SYSTEM/Administrators-only ACL. The viewer uses
`%APPDATA%\MeshRMM\viewer-identity` on Windows and
`~/Library/Application Support/MeshRMM/viewer-identity` on macOS. Unix directories
are owner-only and private-key files are mode 0600; Windows viewer files inherit
the current user's profile permissions. Corrupt or expired identity files are
not replaced silently. Keys expire after five years. Keep private identity files
out of release artifacts and do not copy them between endpoints.

The optional `--identity-fingerprint` command prints the local public certificate
fingerprint without contacting a server. It is a diagnostic, not a connection
prerequisite. The Windows viewer is a GUI application, so Command Prompt returns
before its output appears; pipe the command, for example
`meshrmm-remote.exe --identity-fingerprint | more`, or run it from PowerShell. Deploy the acknowledged-close server endpoint before the native
release to enable acknowledged cleanup; no fingerprint provisioning is needed.

The following records describe the earlier manual-enrollment validation. That
admission policy has been replaced by automatic authenticated-session acceptance.

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

### Automatic acceptance validation — September 16, 2026

Supersedes the manual enrollment policy above. macOS viewer/session-transport
Clippy and tests passed; hash-verified Windows workspace Clippy/tests and Agent
release build passed. Tests cover new/replacement peers without enrollment,
explicit local certificate blocks, malformed SDP, real WebRTC data exchange
without trust entries, and rejection when the actual DTLS certificate differs
from signaling. Existing AEAD/cipher and independent-channel tests still pass.

Installed the updated Mac viewer and Windows service through the supported
installers. Windows configuration was preserved and signaling reconnected.
Installed Agent SHA-256:
`aa111449e52f03655508e105e814008c4913004839422fc183e49981e6d92a96`.
Both prior enrollment directories were moved to local backups. With no enrollment
list on either endpoint, a fresh dashboard connection opened without a fingerprint
prompt and passed desktop rendering, remote mouse/keyboard input, bidirectional
chat, and clean disconnect. The Windows service was left online. No release was
published and no Cloudflare setting changed for this revision.
