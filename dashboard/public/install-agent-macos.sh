#!/bin/sh
# Installs the MeshRMM Agent on this Mac. The website's "Add a device" dialog
# shows the command that runs it:
#
#   curl -fsSL https://rmm.example.com/install-agent-macos.sh |
#     sudo /bin/sh -s -- https://rmm.example.com <authorization>
#
# It downloads the server's Agent, which the server signs with the company's
# Developer ID when it's set up to, checks its code signature, and enrolls
# this Mac with the one-time authorization.
set -eu

if [ "$#" -ne 2 ]; then
    echo "Usage: install-agent-macos.sh <server URL> <authorization>" >&2
    exit 2
fi
ORIGIN=${1%/}
AUTHORIZATION=$2
case "$ORIGIN" in
    https://*) ;;
    *) echo "The server URL must use HTTPS." >&2; exit 2 ;;
esac
if [ "$(id -u)" -ne 0 ]; then
    echo "Run the installer with sudo." >&2
    exit 1
fi

WORK=$(mktemp -d /tmp/meshrmm-agent-install.XXXXXX)
trap 'rm -rf -- "$WORK"' EXIT
echo "Downloading the MeshRMM Agent..."
STATUS=$(curl -sSL --proto '=https' -o "$WORK/agent.zip" -w '%{http_code}' \
    "$ORIGIN/downloads/meshrmm-agent-macos.zip")
if [ "$STATUS" != 200 ]; then
    # The server explains itself in {"error": "..."}.
    REASON=$(plutil -extract error raw -o - "$WORK/agent.zip" 2>/dev/null || true)
    echo "The server didn't provide the Agent (HTTP $STATUS). $REASON" >&2
    exit 1
fi
ditto -x -k "$WORK/agent.zip" "$WORK"
if ! codesign --verify --strict --deep "$WORK/MeshRMM Agent.app"; then
    echo "The downloaded Agent's code signature is invalid." >&2
    exit 1
fi
"$WORK/MeshRMM Agent.app/Contents/MacOS/meshrmm-agent" --install "$AUTHORIZATION"
