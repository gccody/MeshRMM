#!/bin/sh
# Builds the macOS viewer as a signed app bundle, archives it in dist/downloads
# and describes it in dist/downloads/artifacts.json (signed when
# MESHRMM_RELEASE_SIGNING_KEY holds the signing key). The viewer learns its
# server from each dashboard link; an optional JSON file becomes the bundle's
# Contents/MacOS/remote.json for local settings. --local builds a viewer with
# automatic updates off and archives nothing.
set -eu

SCRIPT_DIR=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
ROOT_DIR=$(CDPATH= cd -- "$SCRIPT_DIR/.." && pwd)
LOCAL_BUILD=false
if [ "${1:-}" = "--local" ]; then
    LOCAL_BUILD=true
    shift
fi
if [ "$#" -gt 1 ]; then
    echo "Usage: $0 [--local] [viewer-config.json]" >&2
    exit 1
fi
CONFIG_PATH=${1:-}
DOWNLOAD_DIR="$ROOT_DIR/dist/downloads"
. "$SCRIPT_DIR/macos-signing.sh"
CODESIGN_IDENTITY=$(meshrmm_codesign_identity)
BUILD_TARGET=${MESHRMM_BUILD_TARGET:-}
VERSION=$(node "$ROOT_DIR/scripts/release-config.mjs" version)

if [ -n "$CONFIG_PATH" ] && [ ! -f "$CONFIG_PATH" ]; then
    echo "Missing viewer settings: $CONFIG_PATH" >&2
    exit 1
fi

if [ -n "$BUILD_TARGET" ]; then
    cargo build --locked --release --target "$BUILD_TARGET" --manifest-path "$ROOT_DIR/remote/Cargo.toml"
    SOURCE_EXECUTABLE="$ROOT_DIR/target/$BUILD_TARGET/release/meshrmm-remote"
    case "$BUILD_TARGET" in
        aarch64-apple-darwin) UPDATE_TARGET=client-macos-arm64 ; OUTPUT_DIRECTORY=dist/remote-macos ;;
        x86_64-apple-darwin) UPDATE_TARGET=client-macos-x64 ; OUTPUT_DIRECTORY=dist/remote-macos-x64 ;;
        *) echo "Unsupported macOS build target: $BUILD_TARGET" >&2; exit 1 ;;
    esac
else
    cargo build --locked --release --manifest-path "$ROOT_DIR/remote/Cargo.toml"
    SOURCE_EXECUTABLE="$ROOT_DIR/target/release/meshrmm-remote"
    OUTPUT_DIRECTORY=dist/remote-macos
    case "$(uname -m)" in
        arm64) UPDATE_TARGET=client-macos-arm64 ;;
        x86_64) UPDATE_TARGET=client-macos-x64 ;;
        *) echo "Unsupported macOS architecture: $(uname -m)" >&2; exit 1 ;;
    esac
fi

APP_DIR="$ROOT_DIR/$OUTPUT_DIRECTORY/MeshRMM Remote.app"
CONTENTS_DIR="$APP_DIR/Contents"
MACOS_DIR="$CONTENTS_DIR/MacOS"

rm -rf -- "$APP_DIR"
mkdir -p -- "$MACOS_DIR" "$CONTENTS_DIR/Resources"
cp -- "$SOURCE_EXECUTABLE" "$MACOS_DIR/meshrmm-remote"
cp -- "$ROOT_DIR/THIRD_PARTY_NOTICES.txt" "$CONTENTS_DIR/Resources/THIRD_PARTY_NOTICES.txt"
if [ -n "$CONFIG_PATH" ]; then
    cp -- "$CONFIG_PATH" "$MACOS_DIR/remote.json"
fi
if [ "$LOCAL_BUILD" = true ]; then
    node - "$MACOS_DIR/remote.json" <<'JS'
const fs = require('node:fs');
const path = process.argv[2];
const config = fs.existsSync(path) ? JSON.parse(fs.readFileSync(path, 'utf8')) : {};
config.auto_update = false;
fs.writeFileSync(path, JSON.stringify(config, null, 2) + '\n');
JS
fi

cat > "$CONTENTS_DIR/Info.plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "https://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>CFBundleDevelopmentRegion</key>
    <string>en</string>
    <key>CFBundleDisplayName</key>
    <string>MeshRMM Remote</string>
    <key>CFBundleExecutable</key>
    <string>meshrmm-remote</string>
    <key>CFBundleIdentifier</key>
    <string>com.meshrmm.remote</string>
    <key>CFBundleInfoDictionaryVersion</key>
    <string>6.0</string>
    <key>CFBundleName</key>
    <string>MeshRMM Remote</string>
    <key>CFBundlePackageType</key>
    <string>APPL</string>
    <key>CFBundleShortVersionString</key>
    <string>$VERSION</string>
    <key>CFBundleVersion</key>
    <string>1</string>
    <key>LSMinimumSystemVersion</key>
    <string>12.0</string>
    <key>CFBundleURLTypes</key>
    <array>
        <dict>
            <key>CFBundleURLName</key>
            <string>MeshRMM Remote Protocol</string>
            <key>CFBundleURLSchemes</key>
            <array>
                <string>meshrmm</string>
            </array>
        </dict>
    </array>
    <key>NSHighResolutionCapable</key>
    <true/>
</dict>
</plist>
PLIST

meshrmm_sign_app "$CODESIGN_IDENTITY" "$APP_DIR" --deep

if [ "$LOCAL_BUILD" = true ]; then
    echo "Built local viewer (automatic updates disabled): $APP_DIR"
    exit 0
fi

mkdir -p -- "$DOWNLOAD_DIR"
ARCHIVE_PATH="$DOWNLOAD_DIR/meshrmm-remote-${UPDATE_TARGET#client-}.zip"
rm -f -- "$ARCHIVE_PATH"
ditto -c -k --sequesterRsrc --keepParent "$APP_DIR" "$ARCHIVE_PATH"
node "$ROOT_DIR/scripts/release-artifacts.mjs" write "$DOWNLOAD_DIR"

echo "Built the viewer: $APP_DIR"
echo "Download archive: $ARCHIVE_PATH"
