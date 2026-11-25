# phase-0-ws-0.6 — Client transports and targets

- **Type:** feat | **Risk:** high | **Priority:** p2
- **Coder:** claude-code/opus-5-5 | **Date:** 2026-11-25
- **Plan:** .metis/plans/phase-0-plan.md §0.6

## Goal

The `ship` client reaches a daemon over its unix socket (`local`) or over
`ssh HOST ship dial-stdio` (`ssh://`), chosen from named targets in a 0600
config file, checks the API version on every exchange, and serves
`ship projects` as text or as `-o json` matching a recorded fixture.

## Architectural context

- OVERVIEW §2 (three transports, none secured by Ship), §2.3 (`ship-net`
  owns ssh dial-stdio; leaf on `ship-core` only), §3.3 (no address printed
  that does not exist), §6 CLI (`ship target add NAME (ssh://USER@HOST |
  tailnet://HOST | local)`, `ship target use`, `ship projects -o json`,
  `ship dial-stdio`), §8 (client and daemon negotiate the API version on
  connect; incompatible pair fails with the command to fix it).
- ADR-0003 rules 2, 5: socket access is the whole authorisation; Phase 0–1
  ship only local and ssh. ADR-0004 rules 2, 4: `ship projects -o json` is
  contract, covered by a fixture.
- Consumes `ship-core` (`API_VERSION`, `PROJECTS_PATH`, `ProjectList`,
  `ApiError`, `ErrorCode::UnsupportedApiVersion`, `Slug`), `ship-daemon`
  (`start`, `Config`, router in `http.rs`) and `ship-net`'s existing
  listener module (unchanged).
- New external crates (all already in `Cargo.lock` through axum): in
  `ship-net` `tokio` (net, process, io-util, time, rt), `hyper` 1 (client,
  http1), `hyper-util` (tokio), `http-body-util`, `bytes`; in `ship`
  `tokio` (rt), `serde`, `serde_json`, `ship-core`, `ship-net`. Dev:
  `tempfile`, `libc`, `tokio` macros/rt-multi-thread.

Design decisions:
- **API version handshake (daemon side, `ship-daemon/src/http.rs`):** a
  middleware on paths under `/api/` adds response headers
  `Ship-Api-Version: v1` and `Ship-Version: <daemon version>`; a request
  carrying a `Ship-Api-Version` other than the daemon's is answered `426`
  `ApiError unsupported_api_version` with details `client_api_version`,
  `daemon_api_version`, `daemon_version`. A request without the header
  (curl) passes. `/healthz` gets neither header (reveals nothing).
  Header names are constants in `ship-core::api`.
- **API version handshake (client side):** every request sends
  `Ship-Api-Version`. A `426`, or a response whose `Ship-Api-Version` is
  missing or differs, fails with a message naming both versions and the fix:
  older client → "upgrade this client to ship <daemon version> or later";
  older daemon → "upgrade ship on the daemon's host and restart it
  (`sudo systemctl restart ship`)". Versions compare by the number in `vN`.
- **Targets (`ship` crate):** `ship target add NAME URL [--socket PATH]`,
  `ship target use NAME`, `ship target list [-o json]`. NAME is a `Slug`.
  URL is `local` or `ssh://[USER@]HOST[:PORT]`; `tailnet://…` is refused
  with "the tailnet transport arrives in Phase 2 (ADR-0003 rule 5)"; any
  other form is refused listing the accepted ones. `--socket` names the
  daemon socket (local path, or the remote path passed to `ship dial-stdio
  --socket`); default `/run/ship/ship.sock`. Adding the first target makes
  it current. Adding an existing name fails.
  Config file: `$SHIP_CONFIG`, else `$XDG_CONFIG_HOME/ship/config.json`,
  else `~/.config/ship/config.json`; JSON `{ "current", "targets": { name:
  { "url", "socket" } } }`. Written atomically (new 0600 file, fsync,
  rename) into a directory created 0700. No current target → `local` at
  the default socket, and the no-daemon message then also suggests
  `ship target add`.
- **Validation (rejection before anything is stored or spawned):** HOST is
  ASCII letters, digits, `.`, `-`, `_` (or a bracketed IPv6 literal of hex
  digits and `:`), not starting with `-`; USER is letters, digits, `.`,
  `_`, `-`, not starting with `-`; PORT 1–65535; no path, query or
  fragment; `--socket` is absolute and only letters, digits, `/`, `.`,
  `_`, `-` (it travels through the remote login shell).
- **ssh argv (`ship-net`):** `ssh -T -o BatchMode=yes -o
  ConnectTimeout=<timeout> [-p PORT] [-l USER] -- HOST ship dial-stdio
  [--socket PATH]`, spawned directly (no shell), `kill_on_drop`, stdin and
  stdout as the HTTP stream, stderr captured (bounded 8 KiB).
- **Client exchange (`ship-net::client`):** hyper 1 HTTP/1.1 client over a
  `tokio::net::UnixStream` or the ssh child's joined pipes; `Host:
  localhost` (passes the daemon's Host guard), `Connection: close`; body
  capped at 16 MiB; the whole exchange bounded by `--timeout SECS`
  (default 30). Errors are typed: `NoDaemon` (ENOENT/ECONNREFUSED: names
  the socket and `sudo systemctl start ship` / `ship daemon`),
  `PermissionDenied` (EACCES/EPERM: names the group owning the socket,
  falling back to its directory's group, then `ship`, with `sudo usermod
  -aG <group> $USER`), `TimedOut` (names the timeout and the target),
  `SshUnreachable` (ssh exit 255, with ssh's stderr), `ShipNotInstalled`
  (remote exit 127), `Remote` (any other remote failure, with its stderr,
  which carries dial-stdio's own NoDaemon/PermissionDenied text).
- **`ship dial-stdio [--socket PATH]` (`ship-net::dial_stdio`):** connects
  with the same error mapping, then bridges with two threads: stdin →
  socket (on stdin EOF: shut down the socket's write half), socket →
  stdout. Returns when the socket side ends; after stdin EOF it waits at
  most 10 s for the socket side. Errors go to stderr, exit 1.
- **`ship projects [-o text|json] [--timeout SECS]`:** GETs
  `/api/v1/projects` from the current target. `-o json` prints
  `serde_json::to_string_pretty(ProjectList)` + newline (so local and ssh
  give identical bytes); text prints a `SLUG  ID  CREATED` table or
  `No projects.`. A non-2xx response prints the daemon's `ApiError`
  message. Exit codes: 0 success, 1 failure, 2 usage error.

## Declared file scope

- **owned_paths:**
  - crates/ship/Cargo.toml
  - crates/ship/src/
  - crates/ship/tests/cli_client.rs
  - crates/ship/tests/ssh_real.rs
  - crates/ship/tests/cli_version.rs (one line: its unknown-argument test used `projects`, which this slice makes a real command)
  - crates/ship/tests/fixtures/
  - crates/ship-net/
  - crates/ship-core/src/api.rs
  - crates/ship-core/src/lib.rs
  - crates/ship-daemon/src/http.rs
  - crates/ship-daemon/tests/api_version.rs
  - .github/workflows/ci.yml
  - Cargo.lock
  - .metis/briefs/phase-0-ws-0.6.md
- **read_only_paths:** OVERVIEW.md, .metis/adr/, .metis/plans/phase-0-plan.md, Cargo.toml, crates/ship-core/ (other files), crates/ship-store/, crates/ship-docker/, crates/ship-daemon/ (other files), crates/ship/tests/ (other files), crates/ship/build.rs

## Definition of Done

- [ ] `ship projects -o json` over a `local` target against a fresh daemon prints bytes equal to `crates/ship/tests/fixtures/projects-empty.json`.
- [ ] Over an `ssh://` target (a fake `ssh` on PATH that execs `ship dial-stdio`) it prints the same bytes; and in CI, an ignored test run in a dedicated job against a real sshd on the runner prints the same bytes as the local target and the fixture.
- [ ] With no daemon at the target's socket, the client exits 1 and prints a message naming the socket and how to start the daemon (`systemctl start ship` / `ship daemon`).
- [ ] With an `ssh` that never answers, `ship projects --timeout 1` exits 1 within 5 s, says it timed out, and leaves no `ssh` process behind.
- [ ] Connecting to a socket the user cannot access fails with a permission error naming the socket's group (direct and through `ship dial-stdio`).
- [ ] `ship target add` rejects a host or user containing shell metacharacters, spaces, a leading `-` or `@`/`/` tricks, and leaves the config unchanged; a valid target reaches `ssh` with HOST as one argument after `--` (recorded argv equals the expected list).
- [ ] `ship target add x tailnet://h` exits 2 with the Phase 2 message; `target add` / `use` / `list` work, the first target becomes current, the config file is mode 0600 in a 0700 directory, `use` of an unknown name fails.
- [ ] A request with `Ship-Api-Version: v0` gets `426 unsupported_api_version` from the daemon; API responses carry `Ship-Api-Version`; `/healthz` carries neither version header. A client facing a daemon that answers `Ship-Api-Version: v2` exits 1 with the upgrade instruction.
- [ ] `ship dial-stdio` bridges a request to the daemon and exits when stdin closes and when the daemon closes.
- [ ] `ship-net` still depends only on `ship-core` among workspace crates.
- [ ] `metis verify --post` green.

## Test plan

- `crates/ship-net/src/target.rs` unit tests: URL parsing (local, ssh with/without user/port, IPv6, tailnet refused, garbage refused), hostile host/user/socket values rejected, exact ssh argv.
- `crates/ship-net/src/client.rs` / `dial.rs` unit tests: connect-error mapping (missing socket → NoDaemon, refused → NoDaemon, mode 000 socket → PermissionDenied with group name, skipped as root), bridge over a socketpair exits on input EOF and on peer close, version-header extraction.
- `crates/ship-daemon/tests/api_version.rs`: header present on `/api/v1/projects` and `/api/v1/host`, absent on `/healthz`; `v0` → 426 with the ApiError shape and details; no header → 200.
- `crates/ship/src/` unit tests: config load/save round-trip, 0600/0700 modes, first target current, duplicate refused, version-mismatch message for older client and older daemon.
- `crates/ship/tests/cli_client.rs` (built binary; in-process `ship_daemon::start` on a temp socket): projects JSON equals fixture over local and over fake ssh; text output; no-daemon message; permission error names the group; timeout with a hanging fake ssh; hostile `target add` rejected and config unchanged; recorded ssh argv; tailnet refused; target add/use/list; `dial-stdio` bridge and exit; fake daemon answering `Ship-Api-Version: v2` → upgrade instruction.
- `crates/ship/tests/ssh_real.rs` (`#[ignore]`, needs `SHIP_TEST_SSH=user@host`): local vs ssh bytes vs fixture against a real sshd. CI job `ssh` installs openssh-server on the runner, authorises a fresh key, puts the built `ship` on PATH and runs it.
- Gate: `metis verify --post` (`make verify`).

## Out-of-scope touches

- `crates/ship/tests/cli_version.rs:33` — `unknown_arguments_exit_non_zero`
  used `ship projects` as its example of an unknown subcommand (exit 2).
  This slice makes `projects` a real command, so the test now uses
  `no-such-command`. The assertion it makes is unchanged.

## Report

- `ship-core/src/api.rs`: `API_VERSION_HEADER` (`ship-api-version`),
  `SHIP_VERSION_HEADER` (`ship-version`), `api_version_number`; re-exported
  from `lib.rs`.
- `ship-daemon/src/http.rs`: `api_version` middleware on `/api/` paths
  (version headers on every response; `426 unsupported_api_version` with
  `client_api_version` / `daemon_api_version` / `daemon_version` details);
  `/healthz` untouched. New `tests/api_version.rs`.
- `ship-net`: new `target.rs` (`Target`, `SshTarget::ssh_args`,
  `TargetError`, `DEFAULT_SOCKET`), `connect.rs` (`connect_unix`,
  `ConnectError` NoDaemon / PermissionDenied with group / Io,
  `group_name` via `getgrgid_r`), `dial.rs` (`dial_stdio`, `bridge`),
  `client.rs` (`get`, `Response`, `TransportError`). `Cargo.toml` adds
  `ship-core`, `tokio`, `hyper` (client, http1), `hyper-util`,
  `http-body-util`, `bytes`. Still depends only on `ship-core` among
  workspace crates (`dependency_rule` passes).
- `ship`: `src/config.rs` (targets file), `src/client.rs` (`get_json`,
  version check, `incompatibility` message), `src/commands.rs` (`target
  add|use|list`, `projects`, `dial-stdio`); `main.rs` wires them.
  `Cargo.toml` adds `ship-core`, `ship-net`, `serde`, `serde_json`,
  `tokio`, `bytes`; dev `libc`, `tempfile`, `tokio`. Fixture
  `tests/fixtures/projects-empty.json`. Tests `tests/cli_client.rs` (11)
  and `tests/ssh_real.rs` (ignored; real sshd).
- `.github/workflows/ci.yml`: new `ssh` job. It installs and starts
  openssh-server on the runner, authorises a fresh ed25519 key, symlinks
  the built `ship` into `/usr/local/bin`, then runs `ssh_real` with
  `SHIP_TEST_SSH=$USER@localhost`. It has not run yet: it runs on the next
  push to `dev`, and the real-sshd check passes only once it does.
- Decisions beyond the design list:
  - Version mismatch is checked on every response, not on a separate
    handshake request, so each command costs one round trip (gate: projects
    over ssh in under 2 s).
  - `Connection: close` on every client request, so `dial-stdio` exits as
    soon as the daemon finishes rather than after the client's drain.
  - The local and ssh-fake tests start the daemon in-process with
    `ship_daemon::start`, because `ship daemon` refuses non-Linux and this
    was developed on macOS.
- `cargo` and `cargo fmt` needed approval in this session. Tests ran through
  `metis verify`, and rustfmt's diffs were applied by hand.
- `metis verify --post --slice phase-0-ws-0.6`: ALL GREEN (fmt, clippy
  -D warnings; tests: ship 20 unit + 11 cli_client + 2 cli_daemon + 2
  cli_version + 15 dependency_rule + 1 ignored ssh_real; ship-net 31 unit;
  ship-daemon 11 unit + 2 api_version + existing suites; ship-core 43
  unit).
