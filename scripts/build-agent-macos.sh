#!/bin/sh
# Builds the universal (Apple silicon and Intel) macOS Agent as a signed app
# bundle and publishes it as dashboard/public/downloads/meshrmm-agent-macos.zip.
set -eu

SCRIPT_DIR=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
ROOT_DIR=$(CDPATH= cd -- "$SCRIPT_DIR/.." && pwd)
if [ "$#" -gt 0 ]; then
    echo "Usage: $0" >&2
    exit 1
fi
DASHBOARD_DOWNLOAD_DIR="$ROOT_DIR/dashboard/public/downloads"
UPDATE_MANIFEST="$DASHBOARD_DOWNLOAD_DIR/update-manifest.json"
CONFIGURED_DOWNLOAD_ORIGIN=$(node "$ROOT_DIR/scripts/release-config.mjs" download-origin)
DOWNLOAD_ORIGIN=${MESHRMM_DOWNLOAD_ORIGIN:-$CONFIGURED_DOWNLOAD_ORIGIN}
CODESIGN_IDENTITY=${MESHRMM_CODESIGN_IDENTITY:--}
VERSION=$(node "$ROOT_DIR/scripts/release-config.mjs" version)

for TARGET in aarch64-apple-darwin x86_64-apple-darwin; do
    cargo build --locked --release --target "$TARGET" --manifest-path "$ROOT_DIR/agent/Cargo.toml"
done

APP_DIR="$ROOT_DIR/dist/agent-macos/MeshRMM Agent.app"
CONTENTS_DIR="$APP_DIR/Contents"
rm -rf -- "$APP_DIR"
mkdir -p -- "$CONTENTS_DIR/MacOS" "$CONTENTS_DIR/Resources"
lipo -create -output "$CONTENTS_DIR/MacOS/meshrmm-agent" \
    "$ROOT_DIR/target/aarch64-apple-darwin/release/meshrmm-agent" \
    "$ROOT_DIR/target/x86_64-apple-darwin/release/meshrmm-agent"
cp -- "$ROOT_DIR/THIRD_PARTY_NOTICES.txt" "$CONTENTS_DIR/Resources/THIRD_PARTY_NOTICES.txt"

cat > "$CONTENTS_DIR/Info.plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "https://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>CFBundleDevelopmentRegion</key>
    <string>en</string>
    <key>CFBundleDisplayName</key>
    <string>MeshRMM Agent</string>
    <key>CFBundleExecutable</key>
    <string>meshrmm-agent</string>
    <key>CFBundleIdentifier</key>
    <string>com.meshrmm.agent</string>
    <key>CFBundleInfoDictionaryVersion</key>
    <string>6.0</string>
    <key>CFBundleName</key>
    <string>MeshRMM Agent</string>
    <key>CFBundlePackageType</key>
    <string>APPL</string>
    <key>CFBundleShortVersionString</key>
    <string>$VERSION</string>
    <key>CFBundleVersion</key>
    <string>1</string>
    <key>LSMinimumSystemVersion</key>
    <string>12.3</string>
    <key>LSUIElement</key>
    <true/>
</dict>
</plist>
PLIST

if [ "$CODESIGN_IDENTITY" = "-" ]; then
    # Ad-hoc signing makes a development bundle internally consistent, but
    # macOS forgets the Agent's privacy permissions whenever it changes.
    codesign --force --sign - "$APP_DIR"
else
    codesign --force --options runtime --timestamp --sign "$CODESIGN_IDENTITY" "$APP_DIR"
fi

mkdir -p -- "$DASHBOARD_DOWNLOAD_DIR"
ARCHIVE_PATH="$DASHBOARD_DOWNLOAD_DIR/meshrmm-agent-macos.zip"
rm -f -- "$ARCHIVE_PATH"
ditto -c -k --sequesterRsrc --keepParent "$APP_DIR" "$ARCHIVE_PATH"
node "$ROOT_DIR/scripts/update-release-manifest.mjs" \
    "$UPDATE_MANIFEST" \
    agent-macos \
    "$VERSION" \
    "${DOWNLOAD_ORIGIN%/}/downloads/$(basename "$ARCHIVE_PATH")" \
    "$ARCHIVE_PATH"

echo "Built the macOS Agent: $APP_DIR"
echo "Published update archive: $ARCHIVE_PATH"
