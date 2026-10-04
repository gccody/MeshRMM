#!/bin/sh
# Installs the MeshRMM Agent on this Mac. The dashboard's "Add a device" dialog
# shows the command that runs it:
#
#   curl -fsSL https://<company>.meshrmm.com/install-agent-macos.sh |
#     sudo /bin/sh -s -- https://<company>.meshrmm.com <authorization>
#
# It downloads the published Agent, checks it against the release manifest's
# SHA-256, and enrolls this Mac with the one-time authorization.
set -eu

if [ "$#" -ne 2 ]; then
    echo "Usage: install-agent-macos.sh <dashboard URL> <authorization>" >&2
    exit 2
fi
ORIGIN=${1%/}
AUTHORIZATION=$2
case "$ORIGIN" in
    https://*) ;;
    *) echo "The dashboard URL must use HTTPS." >&2; exit 2 ;;
esac
if [ "$(id -u)" -ne 0 ]; then
    echo "Run the installer with sudo." >&2
    exit 1
fi

WORK=$(mktemp -d /tmp/meshrmm-agent-install.XXXXXX)
trap 'rm -rf -- "$WORK"' EXIT
curl -fsSL --proto '=https' "$ORIGIN/downloads/update-manifest.json" -o "$WORK/manifest.json"
URL=$(plutil -extract releases.agent-macos.url raw -o - "$WORK/manifest.json")
SHA256=$(plutil -extract releases.agent-macos.sha256 raw -o - "$WORK/manifest.json")
case "$URL" in
    https://*) ;;
    *) echo "The release manifest names an unsafe Agent download." >&2; exit 1 ;;
esac
echo "Downloading the MeshRMM Agent..."
curl -fsSL --proto '=https' "$URL" -o "$WORK/agent.zip"
if ! echo "$SHA256  $WORK/agent.zip" | shasum -a 256 -c - >/dev/null; then
    echo "The downloaded Agent failed its SHA-256 integrity check." >&2
    exit 1
fi
ditto -x -k "$WORK/agent.zip" "$WORK"
"$WORK/MeshRMM Agent.app/Contents/MacOS/meshrmm-agent" --install "$AUTHORIZATION"
