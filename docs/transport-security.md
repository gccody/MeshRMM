# Transport security

How the Agent, the viewer and the server protect what travels between them.

## HTTPS and WebSockets

The Agent's and viewer's API and signaling clients require HTTPS and WSS. The
shared URL builder rejects `http` and `ws`, and the authenticated WebSocket
connector refuses a non-WSS URL before it opens a connection or attaches a
credential. Viewer session requests and Agent enrollment are HTTPS-only,
redirects included. Plain WebSockets appear only in unit tests of network
liveness, which don't use the authenticated connector.

Every native HTTPS and WSS client offers **only TLS 1.3**: the Agent's and
viewer's signaling WebSockets, the viewer's session API requests, the Agent's
enrollment request, and both update checks and downloads. A server, or the
proxy in front of it, that offers only TLS 1.2 cannot be reached; see
[running a MeshRMM server](self-hosting.md#tls).
`meshrmm_signaling_client::tls` holds the shared settings. The WebSocket
client trusts the operating system's root certificates, and the Agent's ureq
and the viewer's reqwest clients verify certificates through the operating
system. There is no certificate pinning. Handshake tests against local
TLS 1.2-only servers cover each client.

## The peer connection

Screen, audio, input, clipboard, files and chat use WebRTC data channels,
which WebRTC encrypts with DTLS 1.2; the WebRTC stack has no DTLS 1.3. A TURN
relay forwards the encrypted traffic and cannot read it.

The DTLS defaults allow only ECDHE with AES-128-GCM-SHA256 or
ChaCha20-Poly1305-SHA256. A peer that offers only CBC suites cannot connect.
webrtc 0.14 has no setting for this, so the policy is a small patch to the
`dtls` crate, described with its upgrade notes in
[the vendor note](../vendor/dtls/MESHRMM.md). Each completed handshake logs
the negotiated cipher, and never keys, tokens or payloads.
`crates/session-transport/tests/dtls_security.rs` runs real handshakes: GCM
defaults, ChaCha20-only peers, peers that prefer CBC but support GCM, and
refusal of CBC-only peers in both roles.

## Peer identity

Each Agent and viewer has a persistent certificate. In a session, each peer
accepts the other's certificate when its SHA-256 fingerprint matches the one
the server's signaling delivered, and the DTLS handshake proves the peer
holds its private key. Nobody verifies fingerprints by hand and nothing is
enrolled in advance, new viewers and replaced keys included.

So peer identity rests on the authenticated signaling service. This is not
independent verification against a compromised server, and a changed key is
not detected the way trust-on-first-use would. It does not establish FIPS
certification.

The certificates survive restarts and upgrades:

| | Directory | Protection |
|---|---|---|
| Windows Agent | `%ProgramData%\MeshRMM\Agent\identity` | The installer's SYSTEM and Administrators ACL |
| macOS Agent | `/Library/Application Support/MeshRMM/Agent/identity` | Root only |
| Windows viewer | `%APPDATA%\MeshRMM\viewer-identity` | The user's profile permissions |
| macOS viewer | `~/Library/Application Support/MeshRMM/viewer-identity` | Owner-only directory, key file mode 0600 |

Keys expire after five years. A corrupt or expired identity is not replaced
silently. Keep identity files out of release artifacts and don't copy them
between computers.

Three optional commands work on the local identity, without contacting a
server (`crates/session-transport/src/identity.rs`):

- `--identity-fingerprint` prints the local certificate's fingerprint. The
  Windows viewer is a GUI application, so pipe it:
  `meshrmm-remote.exe --identity-fingerprint | more`.
- `--revoke-peer <fingerprint>` blocks that exact certificate on this
  computer. Close an active session for it to take effect at once.
- `--trust-peer <fingerprint>` removes the block.

No connection needs any of them. Blocking a certificate does not revoke an
account's or a device's access on the server; do that in the website.

## Tokens and credentials

- An Agent's credential is a bearer token the server stores only as a SHA-256
  hash. It lives in the Agent's protected configuration and can be
  [rotated](devices.md#credential-rotation).
- An enrollment authorization works once, for 30 minutes.
- A handoff from the browser to the viewer works once, for 60 seconds. The
  viewer exchanges it for a session token, which only that session accepts.
  An Agent's token cannot end a session.
- TURN credentials are issued per session, signed with the server's instance
  key, and stop working when the session ends.
- The website's sign-in is a server-side session behind an HttpOnly
  `__Host-` cookie, with same-origin checks on every change.

## Updates

Agents and viewers install only builds that carry the release signature, or,
for Mac apps signed with a Developer ID, builds signed by the same team. A
server cannot sign either, so a hostile or compromised server cannot push
code to them. See [releases](releases.md#release-signatures).

## Checking a server's TLS

`scripts/check-transport-security.py` probes a host from outside, without
credentials:

```sh
python3 scripts/check-transport-security.py rmm.example.com
```

It passes when the host offers TLS 1.3, accepts TLS 1.2 only with forward
secrecy and an AEAD cipher, refuses TLS 1.0 and 1.1 and a list of weak
ciphers, and has a valid certificate. That is what the server's own TLS does
in its `acme` and `files` modes. `--minimum-tls 1.3` also fails a host that
accepts TLS 1.2. Port 80 must be closed, as it is on a server without a proxy,
or redirect to HTTPS on the same host. A probe that is inconclusive, such as
a timeout, fails; `--skip-http` leaves out port 80 when a firewall drops it
silently or something other than the server owns it.

For a development server, `--port` names its HTTPS port and `--ca-file` its
development CA or self-signed certificate:

```sh
python3 scripts/check-transport-security.py localhost --port 8443 --ca-file cert.pem
```

The script needs a Python linked to an OpenSSL that can still attempt
TLS 1.0 and 1.1. It tests what is negotiated, not every cipher a server
might accept.
