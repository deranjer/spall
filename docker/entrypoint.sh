#!/bin/sh
# Runs the Spall sandbox server and the download web UI in one container.
# If either process exits, the container stops so the orchestrator restarts it.
set -eu

DATA=${SPALL_DATA_DIR:-/data}
RUN=/run/spall
mkdir -p "$DATA/logs" "$RUN"

# The join token is the shared secret that lets a client into the server. It
# is generated once and kept in the data volume, so restarts keep it stable.
# The web UI hands it to anyone who can reach the page; see docker/README.md.
if [ ! -s "$DATA/join.token" ]; then
    od -An -N32 -tx1 /dev/urandom | tr -d ' \n' > "$DATA/join.token"
    echo "entrypoint: generated a new join token in $DATA/join.token"
fi

# /run persists across container restarts; drop the previous run's files so the web UI
# never serves a stale fingerprint before the new server writes its own.
rm -f "$RUN/server.fingerprint" "$RUN/server.addr"

sandbox-server \
    --serve \
    --listen "0.0.0.0:${SPALL_GAME_PORT:-7777}" \
    --join-token-file "$DATA/join.token" \
    --fingerprint-out "$RUN/server.fingerprint" \
    --addr-out "$RUN/server.addr" \
    --log-json "$DATA/logs/server.jsonl" \
    --ticks 4294967295 \
    --paced \
    --quiescence-ticks 0 \
    --min-clients 0 \
    --max-clients "${SPALL_MAX_CLIENTS:-8}" \
    --worldgen "${SPALL_WORLDGEN:-showcase}" \
    --seed "${SPALL_SEED:-1}" \
    --worldgen-size "${SPALL_WORLD_SIZE:-1024}" \
    --season "${SPALL_SEASON:-summer}" \
    --world "$DATA/world" \
    --save &
server_pid=$!

SPALL_RUN_DIR=$RUN \
SPALL_CLIENT_DIR=/opt/spall/client \
SPALL_JOIN_TOKEN_FILE="$DATA/join.token" \
SPALL_PUBLIC_PORT="${SPALL_PUBLIC_PORT:-${SPALL_GAME_PORT:-7777}}" \
SPALL_PUBLIC_ADDRESS="${SPALL_PUBLIC_ADDRESS:-}" \
SPALL_WEB_PORT="${SPALL_WEB_PORT:-8080}" \
    python3 /opt/spall/webui/app.py &
web_pid=$!

# On docker stop, forward the signal so the server shuts down and saves, then wait for it.
stop_all() {
    kill "$server_pid" "$web_pid" 2>/dev/null || true
    wait "$server_pid" 2>/dev/null || true
    wait "$web_pid" 2>/dev/null || true
    exit 0
}
trap stop_all TERM INT

# Exit as soon as either process ends, and stop the other.
wait_any() {
    while kill -0 "$server_pid" 2>/dev/null && kill -0 "$web_pid" 2>/dev/null; do
        sleep 1
    done
}
wait_any
status=0
kill "$server_pid" "$web_pid" 2>/dev/null || true
wait "$server_pid" 2>/dev/null || status=$?
wait "$web_pid" 2>/dev/null || true
exit "$status"
