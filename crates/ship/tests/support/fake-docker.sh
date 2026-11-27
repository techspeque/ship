#!/bin/sh
# A fake `docker` for the `ship push` tests (cli_push.rs, ssh_real.rs).
# Invocations go to $FAKE_DIR/log. It answers:
# - `compose … config`: $FAKE_DIR/config when it exists, else the first
#   --file;
# - `compose … build`: two lines; when $FAKE_DIR/build_wait exists it
#   touches build_started and waits for build_go, then prints a third;
#   exits with $FAKE_DIR/build_exit;
# - `image inspect`: an image exposing nothing;
# - `compose … up`: records its two --files in $FAKE_DIR/up_files;
# - `ps` and `container inspect`: one running container per Ship service
#   in the override last brought up, of its newest release.
d="$FAKE_DIR"
HEX=4f9a1c0d2e3b4a5968778695a4b3c2d1e0f9a8b7c6d5e4f3a2b1c0d9e8f7a6b
line=""
for a in "$@"; do line="$line[$a]"; done
printf '%s\n' "$line" >> "$d/log"
first="$1"
cmd=""
f1=""
f2=""
prev=""
for a in "$@"; do
  if [ "$prev" = "--file" ]; then
    if [ -z "$f1" ]; then f1="$a"; else f2="$a"; fi
  fi
  if [ -z "$cmd" ]; then
    case "$a" in config|build|up|stop|start|restart|down) cmd="$a";; esac
  fi
  prev="$a"
done
services() {
  l=$(sed -n 2p "$d/up_files")
  grep -o '"dev.ship.service": "svc_[a-z0-9]*"' "$l" | sed 's/.*: "\([^"]*\)"/\1/' | sort -u
}
case "$first" in
  ps)
    case "$line" in
      *"label=dev.ship.environment="*)
        if [ -f "$d/up_files" ]; then
          n=0
          for s in $(services); do n=$((n+1)); echo "$HEX$n"; done
        fi;;
    esac
    exit 0;;
  network) exit 0;;
  image)
    echo "[{\"Id\":\"sha256:${HEX}0\",\"Config\":{\"ExposedPorts\":null,\"Volumes\":null}}]"
    exit 0;;
  container)
    l=$(sed -n 2p "$d/up_files")
    prj=$(grep -m1 '"dev.ship.project": "prj_' "$l" | sed 's/.*: "\([^"]*\)".*/\1/')
    env=$(grep -m1 '"dev.ship.environment": "env_' "$l" | sed 's/.*: "\([^"]*\)".*/\1/')
    rel=$(grep -o '"dev.ship.release": "rel_[a-z0-9]*"' "$l" | sed 's/.*: "\([^"]*\)"/\1/' | sort -u | tail -1)
    out="["
    n=0
    for s in $(services); do
      n=$((n+1))
      if [ "$n" -gt 1 ]; then out="$out,"; fi
      out="$out$(printf '{"Id":"%s%s","Name":"/ship-s%s-1","RestartCount":0,"Config":{"Labels":{"dev.ship.project":"%s","dev.ship.environment":"%s","dev.ship.service":"%s","dev.ship.release":"%s","com.docker.compose.service":"s%s"}},"State":{"Status":"running","ExitCode":0,"OOMKilled":false}}' "$HEX" "$n" "$n" "$prj" "$env" "$s" "$rel" "$n")"
    done
    echo "$out]"
    exit 0;;
  compose) ;;
  *) exit 64;;
esac
case "$cmd" in
  config)
    if [ -f "$d/config" ]; then cat "$d/config"; else cat "$f1"; fi
    exit 0;;
  build)
    echo "#1 building"
    echo "#2 DONE 0.1s" >&2
    if [ -f "$d/build_wait" ]; then
      touch "$d/build_started"
      while [ ! -f "$d/build_go" ]; do sleep 0.05; done
      echo "#3 resumed"
    fi
    exit "$(cat "$d/build_exit" 2>/dev/null || echo 0)";;
  up)
    printf '%s\n%s\n' "$f1" "$f2" > "$d/up_files"
    echo "Container started"
    exit 0;;
esac
exit 0
