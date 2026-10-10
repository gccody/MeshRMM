#!/bin/sh
# Tests a packed server release: installs the tarball with its install.sh and
# runs it under systemd, then runs the Docker image, and checks that each
# answers /healthz and serves the builds artifacts.json lists.
#
#   sudo scripts/test-server-package.sh <output>
#   sudo scripts/test-server-package.sh --tarball <output>
#   scripts/test-server-package.sh --image <output>
#
# <output> is the directory scripts/package-server.sh wrote. The tarball test
# needs Linux with systemd and changes the machine, so it's for CI runners and
# disposable machines; scripts/test-server-package-local.sh runs it in a
# container. The image test needs only Docker, and leaves the image
# meshrmm-server:package-test behind.
set -eu

PART=all
case "${1:-}" in
    --tarball) PART=tarball; shift ;;
    --image) PART=image; shift ;;
esac
if [ "$#" -ne 1 ]; then
    echo "Usage: $0 [--tarball|--image] <output>" >&2
    exit 2
fi
if [ "$PART" != image ] && [ "$(id -u)" -ne 0 ]; then
    echo "The tarball test installs the server; run it as root, or test only the image with --image." >&2
    exit 2
fi
OUTPUT=$(CDPATH= cd -- "$1" && pwd)
SCRIPT_DIR=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
ROOT_DIR=$(CDPATH= cd -- "$SCRIPT_DIR/.." && pwd)
case "$(uname -m)" in
    x86_64) ARCH=x86_64 IMAGE_ARCH=amd64 ;;
    aarch64 | arm64) ARCH=aarch64 IMAGE_ARCH=arm64 ;;
    *) echo "Unsupported architecture: $(uname -m)" >&2; exit 2 ;;
esac
WORK=$(mktemp -d)
CONTAINER=meshrmm-package-test
cleanup() {
    status=$?
    if [ "$PART" != image ]; then
        if [ "$status" -ne 0 ]; then
            journalctl -u meshrmm-server --no-pager -n 50 || true
        fi
        systemctl stop meshrmm-server 2>/dev/null || true
    fi
    if [ "$PART" != tarball ]; then
        if [ "$status" -ne 0 ]; then
            docker logs "$CONTAINER" 2>&1 | tail -n 50 || true
        fi
        docker rm -f "$CONTAINER" >/dev/null 2>&1 || true
        docker volume rm meshrmm-package-test >/dev/null 2>&1 || true
    fi
    rm -rf -- "$WORK"
    exit "$status"
}
trap cleanup EXIT

# Waits for the server at $1 to report itself healthy.
wait_healthy() {
    attempt=0
    until curl -fsS "$1/healthz" 2>/dev/null | grep -q '"status":"ok"'; do
        attempt=$((attempt + 1))
        if [ "$attempt" -ge 30 ]; then
            echo "The server at $1 never became healthy." >&2
            return 1
        fi
        sleep 1
    done
}

# Checks that the server at $1 offers exactly the builds in artifacts.json $2,
# pointing at its public URL, and serves each one intact.
check_downloads() {
    curl -fsS "$1/downloads/update-manifest.json" -o "$WORK/manifest.json"
    python3 - "$1" "$2" "$WORK/manifest.json" <<'PY'
import hashlib, json, sys, urllib.request
server, artifacts_path, manifest_path = sys.argv[1:]
artifacts = json.load(open(artifacts_path))
manifest = json.load(open(manifest_path))
assert manifest["schema_version"] == 3, manifest
assert manifest["releases"].keys() == artifacts["artifacts"].keys(), manifest
for target, artifact in artifacts["artifacts"].items():
    release = manifest["releases"][target]
    assert release["version"] == artifacts["version"], release
    assert release["url"] == f"https://rmm.example.com/downloads/release/{artifact['file']}", release
    assert release["sha256"] == artifact["sha256"], release
    assert release.get("signature") == artifact.get("signature"), release
    assert "developer_id" not in release, release
    # Without macOS signing, the installer is the release build.
    for path in (f"release/{artifact['file']}", artifact["file"]):
        body = urllib.request.urlopen(f"{server}/downloads/{path}").read()
        assert hashlib.sha256(body).hexdigest() == artifact["sha256"], path
print(f"{server} serves {len(manifest['releases'])} builds of {artifacts['version']}")
PY
}

test_tarball() {
    TARBALL=$(ls "$OUTPUT"/meshrmm-server-*-linux-"$ARCH".tar.gz)
    echo "Installing $TARBALL"
    tar -xzf "$TARBALL" -C "$WORK"
    RELEASE=$(echo "$WORK"/meshrmm-server-*)
    "$RELEASE/install.sh"
    cat > /etc/meshrmm/server.toml <<'EOF'
public_url = "https://rmm.example.com"
tls.mode = "proxy"
http.listen = "127.0.0.1:8080"
turn.enabled = false
EOF
    meshrmm-server check-config
    /usr/libexec/meshrmm/rcodesign --version
    systemctl start meshrmm-server
    wait_healthy http://127.0.0.1:8080
    check_downloads http://127.0.0.1:8080 /usr/share/meshrmm/downloads/artifacts.json
    test "$(stat -c %U /var/lib/meshrmm/instance.key)" = meshrmm
    # Installing again upgrades in place and keeps the configuration.
    "$RELEASE/install.sh"
    grep -q 127.0.0.1:8080 /etc/meshrmm/server.toml
    systemctl restart meshrmm-server
    wait_healthy http://127.0.0.1:8080
    systemctl stop meshrmm-server
}

# Starts the image's server on a free local port and waits for it.
run_container() {
    docker run -d --name "$CONTAINER" -p 127.0.0.1::8080 \
        -v meshrmm-package-test:/var/lib/meshrmm \
        -e MESHRMM_PUBLIC_URL=https://rmm.example.com \
        -e MESHRMM_TLS__MODE=proxy \
        -e MESHRMM_HTTP__LISTEN=0.0.0.0:8080 \
        -e MESHRMM_TURN__ENABLED=false \
        meshrmm-server:package-test >/dev/null
    SERVER="http://$(docker port "$CONTAINER" 8080/tcp)"
    wait_healthy "$SERVER"
}

test_image() {
    echo "Running the $IMAGE_ARCH Docker image"
    docker buildx build --platform "linux/$IMAGE_ARCH" --load -t meshrmm-server:package-test \
        -f "$ROOT_DIR/packaging/Dockerfile" "$OUTPUT/image"
    docker run --rm --entrypoint /usr/libexec/meshrmm/rcodesign meshrmm-server:package-test --version
    run_container
    check_downloads "$SERVER" "$OUTPUT/image/linux-$IMAGE_ARCH/share/meshrmm/downloads/artifacts.json"
    # The volume keeps the database across a new container.
    docker exec "$CONTAINER" /usr/bin/meshrmm-server admin create-user admin@example.com >/dev/null
    docker rm -f "$CONTAINER" >/dev/null
    run_container
    if docker logs "$CONTAINER" 2>&1 | grep -q "no accounts exist yet"; then
        echo "The new container lost the account the first one created." >&2
        exit 1
    fi
}

case "$PART" in
    all)
        test_tarball
        test_image
        echo "The tarball and the Docker image both work."
        ;;
    tarball)
        test_tarball
        echo "The tarball works."
        ;;
    image)
        test_image
        echo "The Docker image works."
        ;;
esac
