# MeshRMM TURN nonce patch

Source: crates.io `turn` 0.11.0, the version required by `webrtc` 0.14.0.
Upstream crate SHA-256:
`5ed995882f66ab94238de77c62e5e778389698ab700afa4696f4754da8f457cb`.
The upstream Rust sources, normalized/original Cargo manifests, README, and
MIT/Apache licenses are retained. The examples and benchmark are not, and their
targets are removed from the normalized manifest.

Changes from upstream, all in `src/server/request.rs`:

- The server's nonces are stateless: the issue time followed by its
  HMAC-SHA256 under a key generated once per process. `respond_with_nonce` no
  longer records the nonces it issues, and `authenticate_request` accepts one
  that verifies and is younger than `NONCE_LIFETIME` (one hour).
- Upstream recorded every issued nonce in `Server::nonces` and removed one only
  when a request presented it again. Every unauthenticated Allocate, Refresh,
  CreatePermission or ChannelBind request adds a nonce, so a flood of them
  (with spoofed sources, over UDP) grew the server's memory without bound.
  `turn` 0.17.2 still does this.
- The `nonces` map stays for API compatibility and upstream's tests, which
  insert nonces into it directly. Those are still accepted.

Only the server half changes. The Agent and viewer use this crate's TURN client
through `webrtc`, which is unaffected.

The new unit test is `test_stateless_nonces` in `src/server/request/request_test.rs`.
The crate is outside the workspace, so run its tests directly:
`cd vendor/turn && CARGO_TARGET_DIR=/tmp/turn-target cargo test --lib`.
`server/tests/remote_sessions.rs` exercises the patched authentication end to
end through the MeshRMM server.

When upgrading WebRTC/TURN, check whether upstream still keeps issued nonces,
reapply this patch if so, and remove the vendor override once it doesn't.
