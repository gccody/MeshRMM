#!/bin/sh
# Installs or upgrades the MeshRMM server from an unpacked release tarball:
#
#   sudo ./install.sh
#
# It installs /usr/bin/meshrmm-server, replaces the Agent and viewer builds in
# /usr/share/meshrmm/downloads, installs the systemd unit, and creates the
# meshrmm user. An existing /etc/meshrmm/server.toml is left as it is; a new
# install gets a copy of the example to edit. It doesn't start or restart the
# server.
set -eu

HERE=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
if [ "$(id -u)" -ne 0 ]; then
    echo "Run the installer as root, for example with sudo." >&2
    exit 1
fi
for path in bin/meshrmm-server share/meshrmm/downloads/artifacts.json \
    lib/systemd/system/meshrmm-server.service etc/meshrmm/server.example.toml; do
    if [ ! -e "$HERE/$path" ]; then
        echo "$HERE/$path is missing; run install.sh from the unpacked release." >&2
        exit 1
    fi
done

if ! getent group meshrmm >/dev/null; then
    groupadd --system meshrmm
fi
if ! getent passwd meshrmm >/dev/null; then
    useradd --system --gid meshrmm --home-dir /var/lib/meshrmm --no-create-home \
        --shell /usr/sbin/nologin --comment "MeshRMM server" meshrmm
fi

install -D -m 0755 "$HERE/bin/meshrmm-server" /usr/bin/meshrmm-server.new
mv -f /usr/bin/meshrmm-server.new /usr/bin/meshrmm-server

# Swap the whole downloads directory, so the server never sees a mix of two
# releases' builds.
install -d -m 0755 /usr/share/meshrmm
rm -rf /usr/share/meshrmm/downloads.new /usr/share/meshrmm/downloads.old
cp -R "$HERE/share/meshrmm/downloads" /usr/share/meshrmm/downloads.new
chmod -R u=rwX,go=rX /usr/share/meshrmm/downloads.new
if [ -d /usr/share/meshrmm/downloads ]; then
    mv /usr/share/meshrmm/downloads /usr/share/meshrmm/downloads.old
fi
mv /usr/share/meshrmm/downloads.new /usr/share/meshrmm/downloads
rm -rf /usr/share/meshrmm/downloads.old

install -d -m 0750 -g meshrmm /etc/meshrmm
install -m 0644 "$HERE/etc/meshrmm/server.example.toml" /etc/meshrmm/server.example.toml
first_install=false
if [ ! -e /etc/meshrmm/server.toml ]; then
    install -m 0640 -g meshrmm "$HERE/etc/meshrmm/server.example.toml" /etc/meshrmm/server.toml
    first_install=true
fi

if [ -d /run/systemd/system ]; then
    install -D -m 0644 "$HERE/lib/systemd/system/meshrmm-server.service" \
        /usr/lib/systemd/system/meshrmm-server.service
    systemctl daemon-reload
fi

echo "Installed MeshRMM server $(/usr/bin/meshrmm-server --version | cut -d' ' -f2)."
if [ "$first_install" = true ]; then
    echo "Edit /etc/meshrmm/server.toml, check it with 'meshrmm-server check-config',"
    echo "then start the server with 'systemctl enable --now meshrmm-server'."
else
    echo "Restart the server to finish upgrading: systemctl restart meshrmm-server"
fi
