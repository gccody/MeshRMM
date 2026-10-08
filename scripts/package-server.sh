#!/bin/sh
# Packs a server release for one architecture, on Linux with GNU tar:
#
#   scripts/package-server.sh <x86_64|aarch64> <meshrmm-server> <downloads> <output>
#
# <downloads> holds the Agent and viewer builds and their artifacts.json (see
# scripts/release-artifacts.mjs). The script writes
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
case "$ARCH" in
    x86_64) IMAGE_ARCH=amd64 ;;
    aarch64) IMAGE_ARCH=arm64 ;;
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
