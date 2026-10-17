#!/bin/sh
# Installs the MeshRMM Agent on this Linux computer as a systemd service. The
# website's "Add a device" dialog shows the command that runs it:
#
#   curl -fsSL https://rmm.example.com/install-agent-linux.sh |
#     sudo sh -s -- https://rmm.example.com <authorization>
#
# It downloads the server's Agent for this computer's architecture and enrolls
# this computer with the one-time authorization. It's plain POSIX sh, so it
# runs under dash and BusyBox too.
set -eu

if [ "$#" -ne 2 ]; then
    echo "Usage: install-agent-linux.sh <server URL> <authorization>" >&2
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
# The Agent runs as a systemd service; this is how systemd itself checks
# that it's the running init system.
if [ ! -d /run/systemd/system ]; then
    echo "The MeshRMM Agent needs systemd, which isn't running on this computer." >&2
    exit 1
fi
case "$(uname -m)" in
    x86_64 | amd64) ARCH=x64 ;;
    aarch64 | arm64) ARCH=arm64 ;;
    *) echo "The MeshRMM Agent doesn't support $(uname -m) computers." >&2; exit 1 ;;
esac

# /tmp is often mounted noexec on servers, and the download has to run.
WORK=$(mktemp -d /var/tmp/meshrmm-agent-install.XXXXXX)
trap 'rm -rf -- "$WORK"' EXIT
URL="$ORIGIN/downloads/meshrmm-agent-linux-$ARCH"
echo "Downloading the MeshRMM Agent..."
if command -v curl >/dev/null 2>&1; then
    STATUS=$(curl -sSL --proto '=https' -o "$WORK/meshrmm-agent" -w '%{http_code}' "$URL")
    if [ "$STATUS" != 200 ]; then
        # The server explains itself in {"error": "..."}.
        REASON=$(sed -n 's/.*"error" *: *"\([^"]*\)".*/\1/p' "$WORK/meshrmm-agent" 2>/dev/null || true)
        echo "The server didn't provide the Agent (HTTP $STATUS). $REASON" >&2
        exit 1
    fi
elif command -v wget >/dev/null 2>&1; then
    if ! wget -q -O "$WORK/meshrmm-agent" "$URL"; then
        echo "The server didn't provide the Agent." >&2
        exit 1
    fi
else
    echo "Install curl or wget, then run the installer again." >&2
    exit 1
fi
chmod 755 "$WORK/meshrmm-agent"
"$WORK/meshrmm-agent" --install "$AUTHORIZATION"
