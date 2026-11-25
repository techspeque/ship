#!/usr/bin/env bash
# Shows that a built ship binary can run its daemon: `ship daemon` on a
# temporary state directory, socket and port must answer GET /healthz with
# 200 over the unix socket while the process is still alive, and SIGTERM
# must stop it and remove the socket (plan §0.4).
#
# The socket file appearing is not enough: a daemon that binds and then
# fails (fix-0004: the musl build lost O_PATH on the socket pin) creates and
# removes it in an instant.
#
# Linux only; needs curl. Usage: scripts/daemon-smoke.sh PATH/TO/ship

set -euo pipefail

if [ $# -ne 1 ]; then
	echo "usage: $0 PATH/TO/ship" >&2
	exit 2
fi
bin=$(realpath "$1")
command -v curl >/dev/null || { echo "daemon-smoke: curl is required" >&2; exit 2; }

work=$(mktemp -d)
daemon_pid=""
cleanup() {
	if [ -n "$daemon_pid" ]; then
		kill -TERM "$daemon_pid" 2>/dev/null || true
		wait "$daemon_pid" 2>/dev/null || true
	fi
	rm -rf "$work"
}
trap cleanup EXIT

fail() {
	echo "daemon-smoke: FAIL: $*" >&2
	echo "--- daemon log" >&2
	cat "$work/daemon.log" >&2 || true
	exit 1
}

socket="$work/ship.sock"
echo "--- ship daemon --state $work/state --socket $socket --port 47124"
"$bin" daemon --state "$work/state" --socket "$socket" --port 47124 \
	>"$work/daemon.log" 2>&1 &
daemon_pid=$!

status=""
for _ in $(seq 1 100); do
	kill -0 "$daemon_pid" 2>/dev/null || fail "the daemon exited before /healthz answered"
	status=$(curl -s -o "$work/healthz" -w '%{http_code}' --max-time 2 \
		--unix-socket "$socket" http://localhost/healthz || true)
	[ "$status" = 200 ] && break
	sleep 0.2
done
[ "$status" = 200 ] || fail "GET /healthz over $socket: status '${status:-none}'"
kill -0 "$daemon_pid" 2>/dev/null || fail "the daemon exited after answering /healthz"
echo "/healthz over the unix socket: 200 $(cat "$work/healthz")"

kill -TERM "$daemon_pid"
rc=0
wait "$daemon_pid" || rc=$?
daemon_pid=""
[ "$rc" -eq 0 ] || fail "the daemon exited $rc on SIGTERM"
[ ! -e "$socket" ] || fail "the socket is still at $socket after SIGTERM"
echo "daemon-smoke: OK"
