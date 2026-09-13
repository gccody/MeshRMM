#!/bin/sh
# Build and install this checkout for the current user; no release publishing.
set -eu

SCRIPT_DIR=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
ROOT_DIR=$(CDPATH= cd -- "$SCRIPT_DIR/.." && pwd)
if [ "${1:-}" = "--help" ]; then
    echo "Usage: $0 [viewer-config.json]"
    echo "Builds a local viewer and installs it in ~/Applications for dashboard launches."
    echo "Automatic updates are disabled; production download assets are untouched."
    exit 0
fi
if [ "$(uname -s)" != Darwin ] || [ "$#" -gt 1 ]; then
    echo "Run on macOS: $0 [viewer-config.json]" >&2
    exit 1
fi

if [ "$#" -eq 1 ]; then
    case "$1" in
        /*) ;;
        *) set -- "$PWD/$1" ;;
    esac
fi

# Always build for this Mac, even when the shell was configured for a release
# cross-build. Let the build wrapper select the host architecture and toolchain.
cd "$ROOT_DIR"
MESHRMM_BUILD_TARGET= MESHRMM_CODESIGN_IDENTITY=- \
    sh "$SCRIPT_DIR/build-remote-macos.sh" --local "$@"

SOURCE_APP="$ROOT_DIR/dist/remote-macos/MeshRMM Remote.app"
INSTALL_DIR="$HOME/Applications"
APP_PATH="$INSTALL_DIR/MeshRMM Remote.app"
mkdir -p "$INSTALL_DIR"
STAGING_DIR=$(mktemp -d "$INSTALL_DIR/.meshrmm-install.XXXXXX")
cleanup() {
    rm -rf -- "$STAGING_DIR"
}
trap cleanup EXIT
trap 'exit 1' HUP INT TERM
ditto "$SOURCE_APP" "$STAGING_DIR/MeshRMM Remote.app"
codesign --verify --deep --strict "$STAGING_DIR/MeshRMM Remote.app"
plutil -lint "$STAGING_DIR/MeshRMM Remote.app/Contents/Info.plist"

# Finish building and verifying before interrupting any running viewer.
VIEWER_PIDS=$(pgrep -u "$(id -u)" -x meshrmm-remote || true)
if [ -n "$VIEWER_PIDS" ]; then
    echo "Closing the running viewer to install the local build."
    for viewer_pid in $VIEWER_PIDS; do
        kill -TERM "$viewer_pid" 2>/dev/null || true
    done
    attempts=0
    while pgrep -u "$(id -u)" -x meshrmm-remote >/dev/null; do
        attempts=$((attempts + 1))
        if [ "$attempts" -ge 10 ]; then
            echo "Viewer did not exit. Quit it and rerun this script." >&2
            exit 1
        fi
        sleep 1
    done
fi

BACKUP_DIR=
LSREGISTER=/System/Library/Frameworks/CoreServices.framework/Frameworks/LaunchServices.framework/Support/lsregister
if [ -e "$APP_PATH" ]; then
    BACKUP_DIR=$(mktemp -d "$INSTALL_DIR/.meshrmm-backup.XXXXXX")
    "$LSREGISTER" -u "$APP_PATH" || true
    mv "$APP_PATH" "$BACKUP_DIR/MeshRMM Remote.app"
fi
if ! mv "$STAGING_DIR/MeshRMM Remote.app" "$APP_PATH"; then
    if [ -n "$BACKUP_DIR" ]; then
        mv "$BACKUP_DIR/MeshRMM Remote.app" "$APP_PATH"
    fi
    exit 1
fi

# Avoid registering both the build output and the installed copy as handlers.
"$LSREGISTER" -u "$SOURCE_APP" || true
"$LSREGISTER" -f "$APP_PATH"
echo "Installed: $APP_PATH"
echo "Use a fresh Connect link from the dashboard to test the viewer."
if [ -n "$BACKUP_DIR" ]; then
    echo "Previous viewer retained at: $BACKUP_DIR/MeshRMM Remote.app"
fi
