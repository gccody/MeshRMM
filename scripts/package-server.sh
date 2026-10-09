#!/bin/sh
# Packs a server release for one architecture, on Linux with GNU tar:
#
#   scripts/package-server.sh <x86_64|aarch64> <meshrmm-server> <downloads> <output>
#
# <downloads> holds the Agent and viewer builds and their artifacts.json (see
# scripts/release-artifacts.mjs). The tarball also gets rcodesign, which signs
# the macOS builds with the company's Developer ID when the server is set up
# to (see server/src/downloads/developer_id.rs). The script writes
# <output>/meshrmm-server-<version>-linux-<arch>.tar.gz, and the same files
# unpacked in <output>/image/linux-<amd64|arm64>/ for packaging/Dockerfile.
set -eu

if [ "$#" -ne 4 ]; then
    echo "Usage: $0 <x86_64|aarch64> <meshrmm-server> <downloads> <output>" >&2
    exit 2
fi
ARCH=$1
SERVER=$2
DOWNLOADS=$3
OUTPUT=$4
# rcodesign's release builds, pinned by SHA-256.
RCODESIGN_VERSION=0.29.0
case "$ARCH" in
    x86_64)
        IMAGE_ARCH=amd64
        RCODESIGN_SHA256=dbe85cedd8ee4217b64e9a0e4c2aef92ab8bcaaa41f20bde99781ff02e600002
        ;;
    aarch64)
        IMAGE_ARCH=arm64
        RCODESIGN_SHA256=4af92c87ddf52f5f2d1258a3b4e56c7dcb8f1b2468df744976c5f139e031961f
        ;;
    *) echo "Unsupported architecture: $ARCH" >&2; exit 2 ;;
esac
SCRIPT_DIR=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
ROOT_DIR=$(CDPATH= cd -- "$SCRIPT_DIR/.." && pwd)
VERSION=$(node "$ROOT_DIR/scripts/release-config.mjs" version)
NAME="meshrmm-server-$VERSION-linux-$ARCH"
if [ ! -f "$DOWNLOADS/artifacts.json" ]; then
    echo "$DOWNLOADS has no artifacts.json." >&2
    exit 1
fi

TREE="$OUTPUT/$NAME"
rm -rf -- "$TREE"
RCODESIGN="apple-codesign-$RCODESIGN_VERSION-$ARCH-unknown-linux-musl"
RCODESIGN_WORK=$(mktemp -d)
trap 'rm -rf -- "$RCODESIGN_WORK"' EXIT
curl -fsSL --proto '=https' -o "$RCODESIGN_WORK/$RCODESIGN.tar.gz" \
    "https://github.com/indygreg/apple-platform-rs/releases/download/apple-codesign%2F$RCODESIGN_VERSION/$RCODESIGN.tar.gz"
echo "$RCODESIGN_SHA256  $RCODESIGN_WORK/$RCODESIGN.tar.gz" | sha256sum -c --quiet -
tar -xzf "$RCODESIGN_WORK/$RCODESIGN.tar.gz" -C "$RCODESIGN_WORK"
install -D -m 0755 "$RCODESIGN_WORK/$RCODESIGN/rcodesign" "$TREE/libexec/meshrmm/rcodesign"
install -D -m 0644 "$RCODESIGN_WORK/$RCODESIGN/COPYING" "$TREE/share/doc/meshrmm/rcodesign/COPYING"
printf '%s\n' "rcodesign $RCODESIGN_VERSION (MPL-2.0), from apple-codesign: https://github.com/indygreg/apple-platform-rs" \
    > "$TREE/share/doc/meshrmm/rcodesign/README"
chmod 0644 "$TREE/share/doc/meshrmm/rcodesign/README"
install -D -m 0755 "$SERVER" "$TREE/bin/meshrmm-server"
install -d -m 0755 "$TREE/share/meshrmm/downloads"
cp -- "$DOWNLOADS"/* "$TREE/share/meshrmm/downloads/"
chmod 0644 "$TREE/share/meshrmm/downloads"/*
install -D -m 0644 "$ROOT_DIR/packaging/meshrmm-server.service" \
    "$TREE/lib/systemd/system/meshrmm-server.service"
install -D -m 0644 "$ROOT_DIR/server/server.example.toml" "$TREE/etc/meshrmm/server.example.toml"
install -m 0755 "$ROOT_DIR/packaging/install.sh" "$TREE/install.sh"
install -m 0644 "$ROOT_DIR/THIRD_PARTY_NOTICES.txt" "$TREE/THIRD_PARTY_NOTICES.txt"

# The same bytes for the same commit.
EPOCH=${SOURCE_DATE_EPOCH:-$(git -C "$ROOT_DIR" log -1 --format=%ct)}
tar --sort=name --owner=0 --group=0 --numeric-owner --mtime="@$EPOCH" \
    -C "$OUTPUT" -cf - "$NAME" | gzip -n -9 > "$OUTPUT/$NAME.tar.gz"

IMAGE="$OUTPUT/image"
rm -rf -- "$IMAGE/linux-$IMAGE_ARCH"
mkdir -p -- "$IMAGE/data"
cp -R -- "$TREE" "$IMAGE/linux-$IMAGE_ARCH"
rm -rf -- "$TREE"
echo "Packed $OUTPUT/$NAME.tar.gz"
