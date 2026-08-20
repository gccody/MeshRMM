#!/bin/sh
set -eu

SCRIPT_DIR=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
ROOT_DIR=$(CDPATH= cd -- "$SCRIPT_DIR/.." && pwd)
CONFIG_PATH=${1:-"$ROOT_DIR/dist/remote/remote.json"}
APP_DIR="$ROOT_DIR/dist/remote-macos/PulseRMM Remote.app"
CONTENTS_DIR="$APP_DIR/Contents"
MACOS_DIR="$CONTENTS_DIR/MacOS"

if [ ! -f "$CONFIG_PATH" ]; then
    echo "Missing preconfigured viewer settings: $CONFIG_PATH" >&2
    echo "Run scripts/provision-cloudflare.ps1 on the provisioning machine, then copy remote.json here." >&2
    exit 1
fi

cargo build --release --manifest-path "$ROOT_DIR/remote/Cargo.toml"

rm -rf -- "$APP_DIR"
mkdir -p -- "$MACOS_DIR"
cp -- "$ROOT_DIR/remote/target/release/pulsermm-remote" "$MACOS_DIR/pulsermm-remote"
cp -- "$CONFIG_PATH" "$MACOS_DIR/remote.json"

cat > "$CONTENTS_DIR/Info.plist" <<'PLIST'
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
    <string>0.1.0</string>
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

# Ad-hoc signing makes the locally built bundle internally consistent. A
# Developer ID signature is still required for notarized external distribution.
codesign --force --deep --sign - "$APP_DIR"

echo "Built preconfigured viewer: $APP_DIR"
echo "Cloudflare and device settings are embedded in Contents/MacOS/remote.json."
