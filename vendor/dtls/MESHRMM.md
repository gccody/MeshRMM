# MeshRMM DTLS policy patch

Source: crates.io `dtls` 0.13.0, the version required by `webrtc` 0.14.0.
Upstream crate SHA-256:
`f531dd7c181beaf3cebab3716afa4d0d41ab888be85232583f56bbaf07ca208a`.
The upstream Rust sources, normalized/original Cargo manifests, README, and
MIT/Apache licenses are retained. This is a narrow policy patch, not a new
cryptographic implementation.

Changes from upstream:

- Remove ECDHE-ECDSA/RSA AES-256-CBC-SHA from `default_cipher_suites`.
  Defaults retain ECDHE-ECDSA/RSA AES-128-GCM-SHA256 and
  ECDHE-ECDSA ChaCha20-Poly1305-SHA256, in upstream preference order.
- Expose the negotiated cipher identifier (no key material) on `State` and
  log it after a completed DTLS handshake.

Both MeshRMM peers use these defaults. WebRTC 0.14.0 does not expose a public
DTLS cipher configuration hook. Explicit cipher configuration remains available
in the underlying DTLS crate for interoperability/negative tests; application
code must not override the secure defaults with legacy suites.

Regression handshakes live in `crates/session-transport/tests/dtls_security.rs`
and run with the normal workspace tests. They test current/legacy interoperability,
AEAD negotiation and data exchange, and refusal of CBC-only peers in both roles.
The existing WebRTC channel-isolation integration test also exercises this patch.

When upgrading WebRTC/DTLS, review the upstream diff and cipher defaults, retain
these regression tests, and remove the vendor override if upstream provides an
equivalent policy setting. Do not silently return to upstream CBC defaults.
