#!/usr/bin/env bash
# End-to-end check of `ship service install` / `uninstall` on a disposable
# Linux host with systemd, Docker and passwordless sudo (the CI `service`
# job, or a throwaway VM). It creates the ship user and group and a unit, so
# never run it on a machine you care about.
#
# Usage: scripts/service-smoke.sh PATH/TO/ship
set -euo pipefail

src=${1:?usage: scripts/service-smoke.sh PATH/TO/ship}
ship=/usr/local/bin/ship
unit=/etc/systemd/system/ship.service
sock=/run/ship/ship.sock
state=/var/lib/ship

fail() {
  echo "FAIL: $*" >&2
  sudo systemctl status ship --no-pager >&2 || true
  sudo journalctl -u ship --no-pager -n 50 >&2 || true
  exit 1
}
step() { echo "--- $*"; }

cleanup() {
  docker rm -f ship-smoke >/dev/null 2>&1 || true
  docker volume rm ship-smoke-vol >/dev/null 2>&1 || true
}
trap cleanup EXIT

# Waits until the service has a main PID other than $1 and the socket exists.
wait_running() {
  local old=$1 pid
  for _ in $(seq 1 100); do
    pid=$(systemctl show -p MainPID --value ship)
    if [ "$pid" != 0 ] && [ "$pid" != "$old" ] && sudo test -S "$sock"; then
      echo "$pid"
      return 0
    fi
    sleep 0.2
  done
  fail "daemon not running with a new PID (old $old)"
}

step "install the binary where the ship user can run it"
sudo install -m 0755 "$src" "$ship"

step "a labelled container and volume that uninstall must leave alone"
docker volume create --label dev.ship.project=smoke ship-smoke-vol >/dev/null
docker run -d --name ship-smoke --label dev.ship.project=smoke \
  -v ship-smoke-vol:/data busybox sleep 600 >/dev/null

step "first install"
sudo "$ship" service install
getent group ship >/dev/null || fail "no ship group"
getent passwd ship >/dev/null || fail "no ship user"
id -nG ship | tr ' ' '\n' | grep -qx docker || fail "ship not in docker group"
[ "$(sudo stat -c '%U:%G %a' "$state")" = "ship:ship 700" ] || fail "state dir owner/mode"
[ "$(systemctl is-enabled ship)" = enabled ] || fail "unit not enabled at boot"

step "no password, token or key"
pw=$(sudo getent shadow ship | cut -d: -f2)
case "$pw" in '!'* | '*'*) ;; *) fail "ship has a usable password field: $pw" ;; esac
sudo passwd -S ship | awk '{print $2}' | grep -Eq '^(L|LK)$' || fail "ship account not locked"
if sudo find "$state" -type f | grep -q .; then fail "install created files in $state"; fi

step "start; socket is group ship, mode 0660"
sudo systemctl start ship
pid=$(wait_running 0)
[ "$(sudo stat -c '%G %a' "$sock")" = "ship 660" ] || fail "socket: $(sudo stat -c '%U:%G %a' "$sock")"

step "a login user added to ship reaches the daemon over the socket"
sudo usermod -aG ship "$USER"
out=$(sg ship -c "$ship projects -o json")
echo "$out" | grep -q '"projects"' || fail "projects over the socket: $out"

step "second install changes nothing"
before=$(sha256sum "$unit")
mtime=$(stat -c %Y.%y "$unit")
out=$(sudo "$ship" service install)
echo "$out"
echo "$out" | grep -qx "Nothing changed." || fail "second install reported changes"
[ "$(sha256sum "$unit")" = "$before" ] || fail "unit content changed"
[ "$(stat -c %Y.%y "$unit")" = "$mtime" ] || fail "unit rewritten"
[ "$(systemctl show -p MainPID --value ship)" = "$pid" ] || fail "second install restarted the daemon"

step "restarted after SIGKILL"
sudo kill -KILL "$pid"
pid=$(wait_running "$pid")

step "restarted after SIGTERM from outside systemd"
sudo kill -TERM "$pid"
pid=$(wait_running "$pid")

step "uninstall removes the unit and nothing else"
sudo "$ship" service uninstall
[ ! -e "$unit" ] || fail "unit still present"
systemctl is-active --quiet ship && fail "daemon still running"
sudo test -d "$state" || fail "state dir removed"
sudo find "$state" -name '*.db*' | grep -q . || fail "database removed from $state"
getent passwd ship >/dev/null || fail "ship user removed"
[ "$(docker inspect -f '{{.State.Running}}' ship-smoke)" = true ] || fail "container stopped"
docker volume inspect ship-smoke-vol >/dev/null || fail "volume removed"

step "second uninstall is a no-op"
sudo "$ship" service uninstall | grep -q "nothing to remove" || fail "second uninstall"

step "a unit ship did not write is refused"
printf '[Unit]\nDescription=not ship\n' | sudo tee "$unit" >/dev/null
if sudo "$ship" service install; then fail "install overwrote a foreign unit"; fi
if sudo "$ship" service uninstall; then fail "uninstall removed a foreign unit"; fi
grep -q 'not ship' "$unit" || fail "foreign unit changed"
sudo rm -f "$unit"

echo "service smoke: OK"
