# phase-0-ws-0.4 — Daemon skeleton, listeners and logging

- **Type:** feat | **Risk:** high | **Priority:** p2
- **Coder:** claude-code/opus-5-5 | **Date:** 2026-11-25
- **Plan:** .metis/plans/phase-0-plan.md §0.4

## Goal

`ship daemon [--state DIR]` opens the store, binds only a 0660 unix socket
and a 127.0.0.1 TCP listener, serves `/healthz` and `/api/v1/projects`
through Axum with a same-origin guard on mutating requests, logs structured
JSON to stderr through a secret-redacting writer, and on SIGTERM closes the
database and removes its socket; on non-Linux it refuses to start.

## Architectural context

- OVERVIEW §2.3: `ship-net` (leaf, `ship-core` only) owns listeners;
  `ship-daemon` (top level) owns the Axum API; `ship` dispatches `daemon`.
  §3.3 listener invariant; §5 same-origin check; §6 `/healthz` reveals
  nothing, errors are `ApiError`; §8 structured JSON logs to stderr with
  redaction at the logger.
- ADR-0002 rules 1, 2: Axum, Tokio, SQLx, Clap; `ship daemon` Linux only.
- ADR-0003 rules 1, 2, 5, 6: only unix socket and 127.0.0.1 in Phase 0
  (no tailnet listener); socket 0660; same-origin check for mutating
  browser requests on every transport.
- Consumes `ship-core` (`ApiError`, `ErrorCode`, `ProjectList`,
  `ProjectSummary`, `PROJECTS_PATH`, `SecretValue`, `REDACTED`) and
  `ship-store` (`Store::open`, `list_projects`, `close`).
- New external crates: `tokio` (net, signal, rt, macros, sync, time),
  `axum` 0.8, `tracing`, `tracing-subscriber` (`fmt`, `json`), `clap` 4
  (derive), `serde_json` in `ship-daemon`. Dev: `tempfile`.

Design decisions:
- **`ship-net` listeners:** `bind_unix(path)` and `bind_loopback(port: u16)`.
  The TCP address is fixed to `127.0.0.1`; no API accepts an IP, so
  0.0.0.0 cannot be requested. `bind_unix` binds at `path` owner-only
  (Linux: `fchmod 0600` on the socket before `bind`; the process umask is
  never changed, f-003), then `chmod 0660`, then `listen`, so the socket
  never accepts a connection under a wider mode. An existing `path`: a socket that accepts a
  connection → refuse (another daemon is live); a socket that refuses →
  stale, replaced; anything not a socket → refuse, untouched. Returns a
  guard holding the socket's (dev, ino) that removes the file on cleanup
  only if it is still that socket. Group ownership is the service
  account's primary group (ws-0.8); the daemon does not chown.
- **CLI:** `ship daemon [--state DIR] [--socket PATH] [--port N]`.
  Defaults `/var/lib/ship`, `/run/ship/ship.sock`, `7180`. The state
  directory is created at 0700 if missing; the socket's parent must exist.
  Clap arrives here; `--version`/`-V` output is unchanged and unknown
  subcommands still exit 2.
- **Non-Linux:** `ship daemon` on any OS other than Linux exits 2 with
  "ship daemon runs on Linux only (ADR-0002); this client runs on <os>".
  The `ship-daemon` library itself compiles and is tested on any unix.
- **Run/shutdown:** `ship_daemon::start(config)` binds both listeners
  (failing, and removing anything it bound, if either fails), returns a
  `Running` handle with the bound addresses; `Running::shutdown()` stops
  accepting, drains connections for up to 10 s, closes the store, removes
  the socket. `run(config)` = start + wait for SIGTERM or SIGINT +
  shutdown. Signal handlers are installed before binding.
- **Routes:** `GET /healthz` → `200 {"status":"ok"}`.
  `GET /api/v1/projects` → `ProjectList` from the store. Unknown path →
  `404` `ApiError not_found`; store failure → `500` `ApiError internal`
  with a fixed message (detail logged, not returned).
- **Origin guard (every request, every listener):**
  - `Host`, when present, must name a loopback host (`127.0.0.1`,
    `localhost`, `[::1]`, any port; any port so that `ship open`'s ssh
    forward works). Otherwise `403 forbidden`. This closes DNS rebinding,
    which would otherwise make a hostile page look same-origin.
  - For POST, PUT, PATCH, DELETE: an `Origin` header, when present, must
    equal `http://<Host>`; `Origin: null` is rejected; a `Sec-Fetch-Site`
    other than `same-origin` or `none` is rejected. No `Origin` (CLI
    clients) passes. Rejections are `403 forbidden`.
  - The guard runs before routing, so it also covers routes added later.
- **Logging:** `tracing` + `tracing-subscriber` JSON formatter writing to
  stderr through `RedactingWriter`. A `Redactor` (shared, cloneable)
  holds registered secret values (`register(&SecretValue)`); each log
  record is buffered and every occurrence of a registered value — raw and
  JSON-escaped — is replaced with `[redacted]` before the bytes reach
  stderr. Empty values are ignored. The daemon creates one `Redactor` and
  passes it in `Config`; later slices register secrets on it.
- `/api/v1/host` is ws-0.5; the tailnet listener is Phase 2.

## Declared file scope

- **owned_paths:**
  - crates/ship-net/
  - crates/ship-daemon/
  - crates/ship/Cargo.toml
  - crates/ship/src/
  - crates/ship/tests/cli_daemon.rs
  - Cargo.lock
  - .metis/briefs/phase-0-ws-0.4.md
- **read_only_paths:** OVERVIEW.md, .metis/adr/, .metis/plans/phase-0-plan.md, Cargo.toml, crates/ship-core/, crates/ship-store/, crates/ship/tests/ (other files), crates/ship/build.rs

## Definition of Done

- [ ] A test starts the daemon, enumerates the process's listening sockets from the OS (Linux `/proc`, macOS `lsof`) and fails on any listener other than the daemon's unix socket and `127.0.0.1:<port>`.
- [ ] The socket file has mode 0660; a stale socket is replaced; a live socket or a non-socket file at the path makes start fail and is left in place.
- [ ] If the TCP port cannot be bound, start fails and leaves no socket file.
- [ ] `GET /healthz` returns 200 with a body containing no hostname, version, OS or path.
- [ ] `GET /api/v1/projects` over the unix socket and over TCP returns the store's projects as `ProjectList` (empty on a fresh store; one project after one is created).
- [ ] A POST with a foreign `Origin`, `Origin: null` or `Sec-Fetch-Site: cross-site` is `403` with an `ApiError` `forbidden`; a POST with no Origin or a same-origin Origin passes the guard; a request with a non-loopback `Host` is `403`.
- [ ] A registered test secret (including one containing `"` and `\`) logged through the daemon's logger does not appear on the process's real stderr, while the rest of the record does.
- [ ] SIGTERM makes `run` return, the socket file is gone, and the database is closed (WAL file checkpointed away; reopen succeeds).
- [ ] `ship daemon` on a non-Linux OS exits non-zero with the Linux-only message; `ship --version` and unknown-subcommand behaviour are unchanged.
- [ ] `ship-net` depends only on `ship-core` among workspace crates.
- [ ] `metis verify --post` green.

## Test plan

- `crates/ship-net/src/` unit tests: mode 0660, stale replaced, live refused, regular file refused and untouched, guard removes only its own socket, loopback listener address is 127.0.0.1.
- `crates/ship-daemon/tests/listeners.rs`: `only_unix_socket_and_loopback_listen` (OS enumeration), `tcp_bind_failure_leaves_no_socket`.
- `crates/ship-daemon/tests/http.rs`: healthz body, projects over both transports (empty, then one), 404 shape, origin guard cases, Host guard.
- `crates/ship-daemon/tests/sigterm.rs`: real `kill -TERM` to the test process; `run` returns, socket removed, `ship.db-wal` absent, store reopens.
- `crates/ship-daemon/src/log.rs` unit tests: redaction of raw and JSON-escaped values, empty ignored, record split across writes; `crates/ship-daemon/tests/log_stderr.rs`: re-executes the test binary as a child that logs a registered secret through the real stderr logger; the parent asserts on the captured stderr.
- `crates/ship/tests/cli_daemon.rs`: on non-Linux, `ship daemon` exits 2 with the message; on Linux, `ship daemon --help` succeeds.
- Gate: `metis verify --post` (`make verify`).

## Out-of-scope touches

None.

## Report

- `ship-net`: new `src/listener.rs` (`bind_unix`, `bind_loopback`,
  `SocketFile`, `BindError`, `SOCKET_MODE`); `src/lib.rs` re-exports.
  `Cargo.toml` adds `libc` 0.2 (raw `socket`/`bind`/`listen`); dev `tempfile`.
- `ship-daemon`: `src/lib.rs` (`Config`, `start`, `Running`
  {`socket_path`, `tcp_addr`, `store`, `shutdown`}, `run`, `run_blocking`,
  `DaemonError`, defaults), `src/http.rs` (router, origin guard,
  `error_response`, `HEALTHZ_PATH`), `src/log.rs` (`Redactor`,
  `RedactingMakeWriter`, `subscriber`, `init`, `error`); tests
  `tests/{common/mod.rs,http.rs,listeners.rs,sigterm.rs,log_stderr.rs}`.
  `Cargo.toml` adds `ship-core`, `ship-net`, `ship-store`, `axum` 0.8
  (`http1`, `json`, `tokio`), `tokio`, `tracing`, `tracing-subscriber`
  (`fmt`, `json`), `serde_json`; dev `tempfile`, `tokio` `io-util`.
- `ship`: `src/main.rs` moves to Clap (`daemon` subcommand, manual
  `-V/--version` keeping the existing line); `Cargo.toml` adds
  `ship-daemon`, `clap` 4; new `tests/cli_daemon.rs`. `Cargo.lock` updated.
- Decisions beyond the design list:
  - Socket bound at its final path (owner-only, listening only after
    `chmod 0660`) rather than bound
    elsewhere and renamed: a renamed socket keeps its original bound name,
    which `ss`/`lsof` (and the listener test) would report.
  - Unix-socket liveness probe and stale removal happen before bind; the
    probe-to-bind race with a second daemon starting at the same instant
    is accepted (systemd runs one).
  - A live socket is refused before the store is opened, so a second
    daemon never migrates the first one's database.
  - `run` is exercised in the SIGTERM test on its own thread via
    `run_blocking`: `run`'s future is not `Send` (sqlx higher-ranked
    lifetime limitation), which `ship daemon` does not need.
  - Listener test is Linux (`/proc/self/fd` + `/proc/self/net/*`) and
    macOS (`lsof`); verified locally on macOS only, Linux path runs in CI.
- `cargo`, `make` and `cargo fmt` needed approval in this session; tests
  ran through `metis verify`, and rustfmt's reported diffs were applied
  by hand.
- `metis verify --post --slice phase-0-ws-0.4`: ALL GREEN (fmt, clippy
  -D warnings; tests: ship 5 unit + 2 cli_daemon + 2 cli_version + 15
  dependency_rule; ship-daemon 7 unit + 7 http + 1 listeners + 2
  log_stderr + 1 sigterm; ship-net 10 unit; ship-core and ship-store
  unchanged and passing).
