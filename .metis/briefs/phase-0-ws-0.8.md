# phase-0-ws-0.8 — Service install

- **Type:** feat | **Risk:** medium | **Priority:** p2
- **Coder:** claude-code/opus-5-5 | **Date:** 2026-11-25
- **Plan:** .metis/plans/phase-0-plan.md §0.8

## Goal

`sudo ship service install` sets a Linux host up to run `ship daemon` under
systemd as a dedicated `ship` user: it creates the `ship` user and group,
the 0700 state directory and the runtime socket directory, gives the user
Docker access (and says plainly that this is root-equivalent), and writes
and enables the unit. `ship service uninstall` removes the unit and nothing
else. Both are idempotent and never overwrite a unit they did not write.

## Architectural context

- OVERVIEW §3.1 (a systemd unit runs the daemon as a dedicated `ship` user
  with Docker access), §5 boundaries 3–4 (socket 0660 group `ship`;
  membership = Docker access = root), §8 Install / Uninstall (uninstall
  never removes containers, volumes or the state directory).
- ADR-0002 rule 2 (`ship daemon` is Linux only; the same refusal applies to
  `ship service`), ADR-0003 (socket access is the whole authorisation; no
  tokens or keys of Ship's own).
- Consumes `ship_daemon::{DEFAULT_STATE_DIR, DEFAULT_SOCKET_PATH}` and the
  existing socket binding in `ship-net::listener` (unchanged): the daemon
  creates the socket 0660 with its primary group, which under `User=ship`
  `Group=ship` is `ship`.
- New dependency in `ship`: `libc` 0.2 (already in `Cargo.lock`), for
  `geteuid`.

Design decisions:
- **CLI:** `ship service install` and `ship service uninstall`, no
  options. On non-Linux: exit 2 with the ADR-0002 message. Not root: exit 1,
  "run it as root: sudo ship service install". Exit codes 0 / 1 / 2 as
  elsewhere.
- **Host operations behind a trait** (`service::Host`): run a command with
  structured argv (`std::process::Command`, no shell) and capture
  stdout/exit status. User/group facts come from `getent group ship`,
  `getent passwd ship`, `id -nG ship` (NSS-correct). Paths (unit file,
  state dir, runtime dir) live in a `Layout` struct so tests use a temp
  directory and a fake `Host` that records argv.
- **Preflight before any change:** root, Linux, the `docker` group exists
  (else "install Docker Engine first"), the executable path (from
  `current_exe`, canonicalised) contains only `[A-Za-z0-9/._+-]` (it goes
  unquoted into `ExecStart=`), and an existing unit file carries Ship's
  marker line (else refuse: "… was not written by ship; not overwriting").
- **Install steps, each skipped when already satisfied, each printing
  `created` / `ok` / `updated`:**
  1. group: `groupadd --system ship`
  2. user: `useradd --system --gid ship --home-dir /var/lib/ship
     --no-create-home --shell <nologin> ship` (nologin = first existing of
     `/usr/sbin/nologin`, `/sbin/nologin`, `/bin/false`). No password is
     set, so the account is locked; no token or key is created.
  3. Docker access: `usermod -aG docker ship` unless `id -nG ship` lists
     `docker`. Always printed: "the ship user is in the docker group, which
     is root-equivalent on this host; so is membership of the ship group".
  4. state dir `/var/lib/ship`: created 0700, owner ship:ship; an existing
     one with another mode or owner is corrected (reported `updated`).
  5. runtime dir `/run/ship`: created 0750 ship:ship now; the unit's
     `RuntimeDirectory=ship` / `RuntimeDirectoryMode=0750` recreates it on
     every start (it is tmpfs).
  6. unit `/etc/systemd/system/ship.service`: written atomically (temp file
     + rename, 0644) only when absent or when its content differs; then
     `systemctl daemon-reload`.
  7. `systemctl enable ship.service` unless `systemctl is-enabled
     ship.service` already says `enabled`. Install does not start the
     daemon; it prints `sudo systemctl start ship` and, when the unit
     changed, `sudo systemctl restart ship`, and how to add a login user to
     the `ship` group.
- **Unit content:** first line is the marker `# Written by ship service
  install. ship service uninstall removes it.` `[Unit]` Description,
  `After=network-online.target docker.service`, `Wants=docker.service`.
  `[Service]` `Type=simple`, `User=ship`, `Group=ship`,
  `ExecStart=<exe> daemon --state /var/lib/ship --socket
  /run/ship/ship.sock`, `RuntimeDirectory=ship`,
  `RuntimeDirectoryMode=0750`, `UMask=0077`, `Restart=always`,
  `RestartSec=2`, `TimeoutStopSec=30`, `NoNewPrivileges=yes`,
  `PrivateTmp=yes`, `ProtectSystem=full`. `[Install]`
  `WantedBy=multi-user.target`. `Restart=always` covers both SIGKILL and an
  operator's SIGTERM; `enable` covers reboot. Docker is wanted, not
  required: Docker down is `unknown`, and the daemon stays up.
- **Uninstall:** root + Linux check. No unit → "nothing to remove", exit 0.
  Unit without the marker → refuse, exit 1. Otherwise `systemctl disable
  --now ship.service`, remove the unit file, `systemctl daemon-reload`, and
  print what was left in place (user, group, state directory, containers,
  volumes). Apps keep running under Docker.
- **Docs:** `docs/service.md` — install, start, what it creates, adding a
  login user to `ship` (`sudo usermod -aG ship USER`, re-login) for the ssh
  transport, what that grants, uninstall.

## Declared file scope

- **owned_paths:**
  - crates/ship/Cargo.toml
  - crates/ship/src/main.rs
  - crates/ship/src/service.rs
  - crates/ship/tests/cli_service.rs
  - scripts/service-smoke.sh
  - docs/service.md
  - .github/workflows/ci.yml
  - Cargo.lock
  - .metis/briefs/phase-0-ws-0.8.md
- **read_only_paths:** OVERVIEW.md, .metis/adr/, .metis/plans/phase-0-plan.md, crates/ship-daemon/, crates/ship-net/, crates/ship-core/, crates/ship/src/ (other files), crates/ship/tests/ (other files)

## Definition of Done

- [ ] With a fake `Host` on a clean temp layout, install issues exactly
      `groupadd`, `useradd` (no password option), `usermod -aG docker`,
      `systemctl daemon-reload`, `systemctl enable`, creates the state dir
      0700 and the runtime dir 0750, and writes the unit with the marker.
- [ ] A second install against the resulting state issues no mutating
      command and leaves the unit file's bytes and mtime unchanged.
- [ ] Install refuses a pre-existing unit without the marker and changes
      nothing (no command, file untouched); uninstall refuses it too.
- [ ] Install refuses when the `docker` group is missing, before any change.
- [ ] Uninstall removes only the unit (disable --now, remove, daemon-reload);
      the state dir and its contents remain; a second uninstall is a no-op.
- [ ] The unit text contains `User=ship`, `Group=ship`, `Restart=always`,
      `WantedBy=multi-user.target`, `RuntimeDirectory=ship`, and an
      `ExecStart=` naming the canonical binary path; an executable path with
      a space or `%` is refused.
- [ ] `ship service install` as non-root on Linux exits 1 naming sudo; on
      non-Linux exits 2 with the Linux-only message.
- [ ] CI job `service` on ubuntu-latest runs `scripts/service-smoke.sh`:
      install, start, socket is `ship` group mode 0660 and `ship projects`
      over it works for a user added to `ship`; second install reports no
      change and the unit's checksum is unchanged; SIGKILL of the main PID →
      restarted with a new PID; uninstall leaves `/var/lib/ship` and a
      labelled test container and volume in place; no password is set for
      `ship` (`passwd -S` shows locked). Reboot survival is shown by
      `systemctl is-enabled` = `enabled` in CI and by the Human on the gate
      VM.
- [ ] `metis verify --post` green.

## Test plan

- `crates/ship/src/service.rs` unit tests with `FakeHost` and a temp
  `Layout`: clean install argv and files/modes; idempotent second run;
  foreign unit refused (install and uninstall); missing docker group
  refused with no change; uninstall argv and state preserved; uninstall
  twice; unit rendering; exe-path validation; nologin selection.
- `crates/ship/tests/cli_service.rs` (built binary): non-root (or non-Linux)
  refusal and exit codes; `ship service --help` lists install/uninstall.
  Skipped when run as root.
- `scripts/service-smoke.sh` + CI job `service` (real systemd on the
  runner, via sudo): the acceptance criteria above, end to end.
- Gate: `metis verify --post` (`make verify`).

## Out-of-scope touches

None.

## Report

- `crates/ship/src/service.rs` (new): `Host` trait + `SystemHost`
  (structured argv, stdin null), `Layout`, `install`, `uninstall`,
  `render_unit`, `check_exe_path`, `unsupported`, `not_root`, printers;
  13 unit tests against `FakeHost` and a temp layout.
- `crates/ship/src/main.rs`: `ship service install|uninstall`; Linux check
  (exit 2), root check via `geteuid` (exit 1), binary path from
  `current_exe` canonicalised.
- `crates/ship/Cargo.toml`: `libc` moved from dev-dependencies to
  dependencies (for `geteuid`). `Cargo.lock` unchanged.
- `crates/ship/tests/cli_service.rs` (new, 2 tests): refusal off Linux /
  without root; `service --help`.
- `scripts/service-smoke.sh` (new) and CI job `service` in
  `.github/workflows/ci.yml`: real systemd + Docker on the runner. It has
  not run yet; it runs on the next push to `dev`.
- `docs/service.md` (new): install, what it creates, root-equivalence,
  adding a login user to `ship`, uninstall, foreign units.
- Decisions beyond the design list:
  - Install warns, and does not refuse, when the binary or a directory
    above it is not executable by others (the `ship` user could not start
    it). Refusing would block legitimate group-permission setups.
  - A non-regular file at the unit path, or a non-directory at the state
    or runtime path, is refused and left alone.
  - After `chown`, the mode is set again, because `chown` may clear
    set-id bits and the umask narrows `mkdir`.
- Not verified here: the real-systemd path (this machine is macOS), and
  reboot survival, which the gate VM covers. CI checks `is-enabled`.
- `cargo` and `cargo fmt` needed approval in this session. Tests ran
  through `metis verify`, and rustfmt's diffs were applied by hand.
  `scripts/service-smoke.sh` is not marked executable (chmod needed
  approval); CI runs it with `bash`.
- `metis verify --post --slice phase-0-ws-0.8`: ALL GREEN (fmt, clippy
  -D warnings; ship 33 unit incl. 13 service + 12 cli_client + 2
  cli_daemon + 2 cli_service + 2 cli_version + 15 dependency_rule).
