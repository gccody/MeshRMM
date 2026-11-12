#!/bin/sh
# Builds the Linux viewer for x86_64 and arm64 as
# dist/downloads/meshrmm-remote-linux-x64 and -arm64, and describes the
# downloads folder in dist/downloads/artifacts.json (signed when
# MESHRMM_RELEASE_SIGNING_KEY holds the signing key).
#
# The builds link against glibc 2.28, so they run on every distribution
# released since 2018, and load the desktop libraries they use (X11, Wayland,
# Vulkan, OpenGL, PulseAudio) only when they run. Building needs
# cargo-zigbuild, zig and CMake, and works the same on Linux and macOS hosts.
#
#   scripts/build-remote-linux.sh [x64|arm64]...   # default: both
set -eu

SCRIPT_DIR=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
ROOT_DIR=$(CDPATH= cd -- "$SCRIPT_DIR/.." && pwd)
DOWNLOAD_DIR="$ROOT_DIR/dist/downloads"
GLIBC=2.28

if [ "$#" -eq 0 ]; then
    set -- x64 arm64
fi
mkdir -p -- "$DOWNLOAD_DIR"
for ARCH in "$@"; do
    case "$ARCH" in
        x64) TARGET=x86_64-unknown-linux-gnu ;;
        arm64) TARGET=aarch64-unknown-linux-gnu ;;
        *)
            echo "Usage: $0 [x64|arm64]..." >&2
            exit 1
            ;;
    esac
    cargo zigbuild --locked --release -p meshrmm-remote --target "$TARGET.$GLIBC" \
        --manifest-path "$ROOT_DIR/Cargo.toml"
    OUTPUT="$DOWNLOAD_DIR/meshrmm-remote-linux-$ARCH"
    cp -- "$ROOT_DIR/target/$TARGET/release/meshrmm-remote" "$OUTPUT"
    chmod 755 "$OUTPUT"
    echo "Built the Linux viewer: $OUTPUT"
done
node "$ROOT_DIR/scripts/release-artifacts.mjs" write "$DOWNLOAD_DIR"
