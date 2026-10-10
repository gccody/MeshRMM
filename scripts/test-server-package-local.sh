#!/bin/sh
# Builds a server release from this checkout and tests it on this machine, as
# CI's package job does on its runner:
#
#   scripts/test-server-package-local.sh [downloads]
#
# It needs Docker, Node.js, and cargo-zigbuild with zig; macOS and Linux both
# work. It builds the website and the static Linux server for Docker's
# architecture, packs them with the builds in [downloads] (a directory with an
# artifacts.json, such as dist/downloads; stand-ins without one), and runs
# scripts/test-server-package.sh: the tarball in a disposable privileged
# container that runs systemd, the Docker image on this machine's Docker.
#
# The release stays in dist/package-test and the image as
# meshrmm-server:package-test, beside the test container's image
# (meshrmm-package-test-machine). Nothing is installed on this machine.
set -eu

SCRIPT_DIR=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
ROOT_DIR=$(CDPATH= cd -- "$SCRIPT_DIR/.." && pwd)
if [ "$#" -gt 1 ]; then
    echo "Usage: $0 [downloads]" >&2
    exit 2
fi
DOWNLOADS=${1:+$(CDPATH= cd -- "$1" && pwd)}
if ! command -v cargo-zigbuild >/dev/null 2>&1; then
    echo "cargo-zigbuild is required: pip install ziglang cargo-zigbuild" >&2
    exit 1
fi
ARCH=$(docker info --format '{{.Architecture}}')
case "$ARCH" in
    x86_64 | aarch64) ;;
    *) echo "Unsupported Docker architecture: $ARCH" >&2; exit 1 ;;
esac
TARGET="$ARCH-unknown-linux-musl"
OUTPUT="$ROOT_DIR/dist/package-test"
# The image and the container of a Linux machine with what the two scripts
# use: GNU tar to pack the release, and systemd to run the installed server.
MACHINE=meshrmm-package-test-machine
WORK=$(mktemp -d)
cleanup() {
    status=$?
    docker rm -f "$MACHINE" >/dev/null 2>&1 || true
    rm -rf -- "$WORK"
    exit "$status"
}
trap cleanup EXIT
trap 'exit 1' HUP INT TERM

echo "Building the website and the $ARCH Linux server"
(cd "$ROOT_DIR/dashboard" && npm ci && npm run build)
rustup target add "$TARGET"
(cd "$ROOT_DIR" && cargo zigbuild --locked --release -p meshrmm-server --target "$TARGET")
cp -- "${CARGO_TARGET_DIR:-$ROOT_DIR/target}/$TARGET/release/meshrmm-server" "$WORK/meshrmm-server"

mkdir -- "$WORK/downloads"
if [ -n "$DOWNLOADS" ]; then
    cp -- "$DOWNLOADS"/* "$WORK/downloads/"
else
    (cd "$ROOT_DIR" && node --input-type=module -e '
        import { writeFileSync } from "node:fs";
        import { ARTIFACTS } from "./scripts/release-artifacts.mjs";
        for (const file of Object.values(ARTIFACTS)) writeFileSync(`${process.argv[1]}/${file}`, `${file} stand-in`);
    ' "$WORK/downloads")
    node "$ROOT_DIR/scripts/release-artifacts.mjs" write "$WORK/downloads"
fi

docker build --quiet --tag "$MACHINE" - >/dev/null <<'EOF'
FROM ubuntu:24.04
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates curl nodejs python3 systemd systemd-sysv \
    && rm -rf /var/lib/apt/lists/*
CMD ["/sbin/init"]
EOF

# The same bytes for the same commit; the container can't read a worktree's
# history.
EPOCH=$(git -C "$ROOT_DIR" log -1 --format=%ct)
rm -rf -- "$OUTPUT"
mkdir -p -- "$OUTPUT"
docker run --rm --user "$(id -u):$(id -g)" -e SOURCE_DATE_EPOCH="$EPOCH" \
    -v "$ROOT_DIR:/src:ro" -v "$WORK:/work:ro" -v "$OUTPUT:/release" \
    "$MACHINE" sh /src/scripts/package-server.sh "$ARCH" /work/meshrmm-server /work/downloads /release

docker rm -f "$MACHINE" >/dev/null 2>&1 || true
docker run -d --name "$MACHINE" --privileged --tmpfs /run --tmpfs /run/lock \
    -v "$ROOT_DIR:/src:ro" -v "$OUTPUT:/release:ro" "$MACHINE" >/dev/null
attempt=0
until docker exec "$MACHINE" systemctl is-system-running 2>/dev/null | grep -q -E '^(running|degraded)$'; do
    attempt=$((attempt + 1))
    if [ "$attempt" -ge 30 ]; then
        echo "systemd never started in the test container." >&2
        exit 1
    fi
    sleep 1
done
docker exec "$MACHINE" sh /src/scripts/test-server-package.sh --tarball /release
docker rm -f "$MACHINE" >/dev/null

sh "$SCRIPT_DIR/test-server-package.sh" --image "$OUTPUT"
echo "Release: $OUTPUT"
