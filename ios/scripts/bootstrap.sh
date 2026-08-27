#!/bin/sh
set -eu

SCRIPT_DIRECTORY=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
IOS_DIRECTORY=$(CDPATH= cd -- "$SCRIPT_DIRECTORY/.." && pwd)
VENDOR_DIRECTORY="$IOS_DIRECTORY/Vendor"
FRAMEWORK_DIRECTORY="$VENDOR_DIRECTORY/WebRTC.xcframework"
ARCHIVE_URL="https://github.com/stasel/WebRTC/releases/download/151.0.0/WebRTC-M151.xcframework.zip"
EXPECTED_SHA256="64a218fad3d84a0d783321aa9a1eec58ca266ac7879123f86b0b44b703b7d8dc"

if [ -d "$FRAMEWORK_DIRECTORY" ]; then
    echo "WebRTC 151.0.0 is already installed."
    exit 0
fi

TEMPORARY_DIRECTORY=$(mktemp -d "${TMPDIR:-/tmp}/meshrmm-webrtc.XXXXXX")
trap 'rm -rf -- "$TEMPORARY_DIRECTORY"' EXIT INT TERM
ARCHIVE="$TEMPORARY_DIRECTORY/WebRTC.zip"

echo "Downloading WebRTC 151.0.0…"
curl --fail --location --silent --show-error "$ARCHIVE_URL" --output "$ARCHIVE"
ACTUAL_SHA256=$(shasum -a 256 "$ARCHIVE" | awk '{print $1}')
if [ "$ACTUAL_SHA256" != "$EXPECTED_SHA256" ]; then
    echo "WebRTC checksum mismatch." >&2
    exit 1
fi

mkdir -p "$VENDOR_DIRECTORY"
ditto -x -k "$ARCHIVE" "$TEMPORARY_DIRECTORY/unpacked"
mv "$TEMPORARY_DIRECTORY/unpacked/WebRTC.xcframework" "$FRAMEWORK_DIRECTORY"
echo "Installed $FRAMEWORK_DIRECTORY"
