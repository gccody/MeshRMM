#!/bin/sh
# Runs the server's tests on SQLite and on a throwaway PostgreSQL on this
# machine, which CI gets from its PostgreSQL service:
#
#   scripts/test-server-postgres.sh [cargo test arguments]
#
# It needs PostgreSQL's initdb and pg_ctl, on PATH or where pg_config or
# Debian's packages put them. The cluster lives in a temporary directory,
# listens only on 127.0.0.1 (port 54329, or MESHRMM_TEST_POSTGRES_PORT) and is
# removed when the tests end.
set -eu

SCRIPT_DIR=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
ROOT_DIR=$(CDPATH= cd -- "$SCRIPT_DIR/.." && pwd)
PORT=${MESHRMM_TEST_POSTGRES_PORT:-54329}

if ! command -v initdb >/dev/null 2>&1; then
    if command -v pg_config >/dev/null 2>&1; then
        PATH="$(pg_config --bindir):$PATH"
    else
        for directory in /usr/lib/postgresql/*/bin; do
            if [ -x "$directory/initdb" ]; then
                PATH="$directory:$PATH"
            fi
        done
    fi
fi
if ! command -v initdb >/dev/null 2>&1 || ! command -v pg_ctl >/dev/null 2>&1; then
    echo "PostgreSQL's initdb and pg_ctl are required; install PostgreSQL or add its bin directory to PATH." >&2
    exit 1
fi

WORK=$(mktemp -d)
cleanup() {
    status=$?
    pg_ctl -D "$WORK/data" -m immediate stop >/dev/null 2>&1 || true
    rm -rf -- "$WORK"
    exit "$status"
}
trap cleanup EXIT
trap 'exit 1' HUP INT TERM

# Only this machine can reach the cluster, so it trusts every connection.
initdb -D "$WORK/data" -U postgres -A trust --no-sync >/dev/null
if ! pg_ctl -D "$WORK/data" -l "$WORK/postgres.log" -w start \
    -o "-p $PORT -c listen_addresses=127.0.0.1 -c unix_socket_directories= -c fsync=off" >/dev/null; then
    cat "$WORK/postgres.log" >&2
    echo "PostgreSQL did not start on port $PORT; set MESHRMM_TEST_POSTGRES_PORT to a free port." >&2
    exit 1
fi

cd "$ROOT_DIR"
MESHRMM_TEST_POSTGRES_URL="postgres://postgres@127.0.0.1:$PORT/postgres" \
    cargo test --locked -p meshrmm-server "$@"
