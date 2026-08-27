# MeshRMM for iOS

The iOS app is a native SwiftUI viewer for the existing MeshRMM control plane and Windows Agent. It uses WorkOS PKCE authentication, stores rotating tokens in the iOS Keychain, follows the live company Agent inventory, and opens encrypted WebRTC remote-control sessions. H.264 access units are rendered with `AVSampleBufferDisplayLayer`; no remote pixels traverse the Cloudflare control plane.

## One-time setup

1. In the WorkOS dashboard for the MeshRMM application, add `meshrmm-ios://auth/callback` as an allowed redirect URI. The app uses the existing public client ID and never embeds an API key or client secret.
2. Deploy the server and dashboard Workers so `GET /v1/mobile/config` is available on company subdomains.
3. Download the pinned, checksum-verified WebRTC XCFramework:

   ```sh
   sh ios/scripts/bootstrap.sh
   ```

The downloaded `ios/Vendor/` directory is ignored by Git. The committed Xcode project links WebRTC 151.0.0, whose archive checksum is pinned in the bootstrap script.

## Build and test

Open `ios/MeshRMM.xcodeproj`, choose an iPhone/iPad or simulator, and run the `MeshRMM` scheme. For command-line verification:

```sh
sh ios/scripts/bootstrap.sh
xcodebuild -project ios/MeshRMM.xcodeproj \
  -scheme MeshRMM \
  -destination 'platform=iOS Simulator,name=iPhone 17 Pro' \
  CODE_SIGNING_ALLOWED=NO test
```

The unit tests lock the Swift postcard encoding to Rust-owned protocol fixtures and cover video fragment reassembly, Annex-B conversion, API decoding, and workspace validation.

Use Xcode's **Run** action, or an `xcodebuild build` command without
`CODE_SIGNING_ALLOWED=NO`, for interactive simulator testing. The no-sign option
is suitable for compilation and unit tests only; installing that output strips
the simulated `application-identifier` entitlement and makes Keychain writes
fail with `errSecMissingEntitlement` (`-34018`) after WorkOS authentication.

## Remote controls

- Drag with one finger to move the pointer like a trackpad.
- Tap to left-click, or tap with two fingers to right-click.
- Hold briefly and then drag to hold the left button while moving.
- Pan with two fingers to scroll at the pointer's current position.
- The desktop opens at a readable mobile zoom and follows the pointer when part of the display is cropped.
- A high-contrast local pointer is drawn over the video because Desktop Duplication does not include the Windows cursor in captured frames.
- Use the keyboard button for Windows scan-code keyboard input.
- Use the display button when the endpoint has multiple monitors.

The current mobile viewer intentionally advertises H.264 4:2:0 only, which is the hardware-decoded compatibility profile supported by every enrolled Windows Agent.
