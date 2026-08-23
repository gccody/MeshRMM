#!/bin/sh
set -eu

SCRIPT_DIR=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
ROOT_DIR=$(CDPATH= cd -- "$SCRIPT_DIR/.." && pwd)
if [ "$#" -gt 0 ]; then
    CONFIG_PATH=$1
else
    CONFIG_PATH="$ROOT_DIR/dist/remote/remote.json"
    node "$ROOT_DIR/scripts/release-config.mjs" viewer-config "$CONFIG_PATH"
fi
DASHBOARD_DOWNLOAD_DIR="$ROOT_DIR/dashboard/public/downloads"
UPDATE_MANIFEST="$DASHBOARD_DOWNLOAD_DIR/update-manifest.json"
CONFIGURED_DOWNLOAD_ORIGIN=$(node "$ROOT_DIR/scripts/release-config.mjs" download-origin)
DOWNLOAD_ORIGIN=${MESHRMM_DOWNLOAD_ORIGIN:-$CONFIGURED_DOWNLOAD_ORIGIN}
CODESIGN_IDENTITY=${MESHRMM_CODESIGN_IDENTITY:--}
BUILD_TARGET=${MESHRMM_BUILD_TARGET:-}
VERSION=$(node "$ROOT_DIR/scripts/release-config.mjs" version)

if [ ! -f "$CONFIG_PATH" ]; then
    echo "Missing preconfigured viewer settings: $CONFIG_PATH" >&2
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
mkdir -p -- "$MACOS_DIR"
cp -- "$SOURCE_EXECUTABLE" "$MACOS_DIR/meshrmm-remote"
cp -- "$CONFIG_PATH" "$MACOS_DIR/remote.json"

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

if [ "$CODESIGN_IDENTITY" = "-" ]; then
    # Ad-hoc signing makes a local development bundle internally consistent.
    codesign --force --deep --sign - "$APP_DIR"
else
    codesign --force --deep --options runtime --timestamp --sign "$CODESIGN_IDENTITY" "$APP_DIR"
fi

mkdir -p -- "$DASHBOARD_DOWNLOAD_DIR"
ARCHIVE_PATH="$DASHBOARD_DOWNLOAD_DIR/meshrmm-remote-${UPDATE_TARGET#client-}.zip"
rm -f -- "$ARCHIVE_PATH"
ditto -c -k --sequesterRsrc --keepParent "$APP_DIR" "$ARCHIVE_PATH"
node "$ROOT_DIR/scripts/update-release-manifest.mjs" \
    "$UPDATE_MANIFEST" \
    "$UPDATE_TARGET" \
    "$VERSION" \
    "${DOWNLOAD_ORIGIN%/}/downloads/$(basename "$ARCHIVE_PATH")" \
    "$ARCHIVE_PATH"

echo "Built preconfigured viewer: $APP_DIR"
echo "Cloudflare and device settings are embedded in Contents/MacOS/remote.json."
echo "Published update archive: $ARCHIVE_PATH"
