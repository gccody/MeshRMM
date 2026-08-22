#!/bin/sh
set -eu

SCRIPT_DIR=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
ROOT_DIR=$(CDPATH= cd -- "$SCRIPT_DIR/.." && pwd)
CONFIG_PATH=${1:-"$ROOT_DIR/dist/remote/remote.json"}
APP_DIR="$ROOT_DIR/dist/remote-macos/PulseRMM Remote.app"
CONTENTS_DIR="$APP_DIR/Contents"
MACOS_DIR="$CONTENTS_DIR/MacOS"
DASHBOARD_DOWNLOAD_DIR="$ROOT_DIR/dashboard/public/downloads"
UPDATE_MANIFEST="$DASHBOARD_DOWNLOAD_DIR/update-manifest.json"
DOWNLOAD_ORIGIN=${PULSERMM_DOWNLOAD_ORIGIN:-https://pulsermm.gccody.dev}
CODESIGN_IDENTITY=${PULSERMM_CODESIGN_IDENTITY:--}
VERSION=$(cargo metadata --no-deps --format-version 1 --manifest-path "$ROOT_DIR/Cargo.toml" | node -e 'let input=""; process.stdin.on("data", chunk => input += chunk).on("end", () => console.log(JSON.parse(input).packages.find(pkg => pkg.name === "pulsermm-remote").version))')

if [ ! -f "$CONFIG_PATH" ]; then
    echo "Missing preconfigured viewer settings: $CONFIG_PATH" >&2
    echo "Run scripts/provision-cloudflare.ps1 on the provisioning machine, then copy remote.json here." >&2
    exit 1
fi

cargo build --locked --release --manifest-path "$ROOT_DIR/remote/Cargo.toml"

rm -rf -- "$APP_DIR"
mkdir -p -- "$MACOS_DIR"
cp -- "$ROOT_DIR/target/release/pulsermm-remote" "$MACOS_DIR/pulsermm-remote"
cp -- "$CONFIG_PATH" "$MACOS_DIR/remote.json"

cat > "$CONTENTS_DIR/Info.plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "https://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>CFBundleDevelopmentRegion</key>
    <string>en</string>
    <key>CFBundleDisplayName</key>
    <string>PulseRMM Remote</string>
    <key>CFBundleExecutable</key>
    <string>pulsermm-remote</string>
    <key>CFBundleIdentifier</key>
    <string>com.pulsermm.remote</string>
    <key>CFBundleInfoDictionaryVersion</key>
    <string>6.0</string>
    <key>CFBundleName</key>
    <string>PulseRMM Remote</string>
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
            <string>PulseRMM Remote Protocol</string>
            <key>CFBundleURLSchemes</key>
            <array>
                <string>pulsermm</string>
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
case "$(uname -m)" in
    arm64) UPDATE_TARGET=client-macos-arm64 ;;
    x86_64) UPDATE_TARGET=client-macos-x64 ;;
    *) echo "Unsupported macOS architecture: $(uname -m)" >&2; exit 1 ;;
esac
ARCHIVE_PATH="$DASHBOARD_DOWNLOAD_DIR/pulsermm-remote-${UPDATE_TARGET#client-}.zip"
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
