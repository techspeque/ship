#!/usr/bin/env bash
# Shows that the ship binary makes no network request at start-up or on any
# Phase 0 command that runs without root (ADR-0012 rule 3).
#
# Runs each command under `strace -f`, tracing connect, sendto and sendmsg,
# and fails on any call that names an AF_INET or AF_INET6 address. Unix
# sockets (the daemon's socket, Docker's socket) and netlink are allowed.
# The daemon's own 127.0.0.1 listener is a bind, not a connect, and is not
# reported. `ship service` needs root and touches only local files and
# systemd; `ship open` launches a browser by design; neither runs here.
#
# Linux only; needs strace. Usage: scripts/no-network-smoke.sh PATH/TO/ship

set -euo pipefail

if [ $# -ne 1 ]; then
	echo "usage: $0 PATH/TO/ship" >&2
	exit 2
fi
bin=$(realpath "$1")
command -v strace >/dev/null || { echo "no-network-smoke: strace is required" >&2; exit 2; }

work=$(mktemp -d)
daemon_pid=""
cleanup() {
	if [ -n "$daemon_pid" ]; then
		pkill -TERM -P "$daemon_pid" 2>/dev/null || true
		wait "$daemon_pid" 2>/dev/null || true
	fi
	rm -rf "$work"
}
trap cleanup EXIT

export HOME="$work/home"
export SHIP_CONFIG="$work/config.json"
unset XDG_CONFIG_HOME DOCKER_HOST
mkdir -p "$HOME"
socket="$work/ship.sock"
traces="$work/traces"
mkdir -p "$traces"

trace() {
	local name=$1
	shift
	echo "--- $name: ship $*"
	strace -f -qq -e trace=connect,sendto,sendmsg -o "$traces/$name" "$bin" "$@"
}

trace version --version
trace help --help
trace target-add target add here local --socket "$socket"
trace target-use target use here
trace target-list target list -o json

echo "--- daemon: ship daemon --state $work/state --socket $socket --port 47123"
strace -f -qq -e trace=connect,sendto,sendmsg -o "$traces/daemon" \
	"$bin" daemon --state "$work/state" --socket "$socket" --port 47123 \
	>"$work/daemon.log" 2>&1 &
daemon_pid=$!
for _ in $(seq 1 100); do
	[ -S "$socket" ] && break
	sleep 0.2
done
if [ ! -S "$socket" ]; then
	echo "no-network-smoke: daemon did not create $socket" >&2
	cat "$work/daemon.log" >&2
	exit 1
fi

trace projects projects -o json
printf 'GET /healthz HTTP/1.0\r\nHost: ship\r\n\r\n' |
	trace dial-stdio dial-stdio --socket "$socket"
echo

# Stop the daemon (strace's child) and let strace flush its trace.
pkill -TERM -P "$daemon_pid" || true
wait "$daemon_pid" || true
daemon_pid=""

if grep -HnE 'AF_INET6?[,}]' "$traces"/*; then
	echo "no-network-smoke: FAIL: the calls above reach an IP address" >&2
	exit 1
fi
files=("$traces"/*)
echo "no-network-smoke: no IP connection in ${#files[@]} traced commands"
