# phase-0-ws-0.5 — Docker and Compose preflight

- **Type:** feat | **Risk:** medium | **Priority:** p2
- **Coder:** claude-code/opus-5-5 | **Date:** 2026-11-25
- **Plan:** .metis/plans/phase-0-plan.md §0.5

## Goal

`ship-docker` gets an argv-only, bounded, timed subprocess runner and a
read-only preflight that reports Docker Engine reachability and version and
the Compose v2 plugin version against one supported range; the daemon
serves `/api/v1/host` with that status (`ready`, `unreachable`,
`unsupported_version`), both versions and host capacity (CPUs, memory, free
disk in the state directory), and a CI job runs the preflight tests against
a real Docker Engine.

## Architectural context

- OVERVIEW §2.3: all Docker access lives in `ship-docker` (leaf, depends on
  `ship-core` only); `ship-daemon` serves the API. §3.3: Docker invoked with
  structured argv, never a shell, never user-supplied flags; Ship touches
  no Docker object it did not label. §6: `/api/v1/host`; errors stay
  `ApiError`. §8: Docker down → clear error, daemon keeps running.
- ADR-0001 rules 2, 6: structured argv through `ship-docker`; preflight
  checks Engine and Compose versions against a range validated in CI.
- Consumes `ship-core` (`HostInfo`, `HOST_PATH`, `API_VERSION`, the
  crate-private `string_enum!` macro) and the ws-0.4 daemon (`Config`,
  `start`, `http::router`, test helpers in `tests/common`).
- New external crates: `ship-docker` gets `tokio` (process, io-util, time,
  rt, macros) and `libc` (process-group kill); `ship-daemon` gets
  `ship-docker` and `libc` (`statvfs`). Dev: `tempfile`.

Design decisions:
- **Runner (`ship_docker::Runner`):** holds a program (default `docker`,
  found on `PATH`), environment overrides, a timeout (default 10 s) and an
  output cap per stream (default 1 MiB). Its only execution API is
  `run(args)` taking an iterator of `OsStr`-like arguments, each passed to
  `execve` unchanged. No method accepts a command string; nothing invokes a
  shell. stdin is null; the child runs in its own process group. Exceeding
  the cap or the timeout kills the process group and returns a typed
  error (`OutputTooLarge`, `Timeout`); a missing program is `Spawn` with
  `NotFound`.
- **Preflight (`ship_docker::preflight(&Runner) -> DockerInfo`):** runs
  exactly two read-only commands, concurrently:
  `docker version --format {{.Server.Version}}` and
  `docker compose version --short`. Engine command failing (non-zero exit,
  spawn failure, timeout) → `unreachable`, with a message carrying the
  first line of Docker's stderr and what to do (start Docker; give the
  daemon's user access to the Docker socket). Engine or Compose version
  outside the range, unparsable, or Compose missing → `unsupported_version`,
  with a message naming the detected version and the supported range.
  Otherwise `ready`. `unreachable` wins over `unsupported_version`.
  Version strings are reported only when short and made of version
  characters; parsing takes the leading `major.minor.patch` and ignores
  suffixes such as `-ce` or `+dfsg1`, and a leading `v`.
- **Supported range, in one place (`ship_docker::supported`):** Engine
  `>= 24.0.0, < 30.0.0`; Compose `>= 2.20.0, < 6.0.0` (Compose skipped
  3 and 4 and released 5). Unit tests cover the boundaries; the CI Docker
  job asserts the runner's real Engine and Compose are `ready`, so a CI
  image outside the range fails the build.
- **Wire (`ship-core::api`):** `HostInfo` gains `docker: DockerInfo` and
  `capacity: HostCapacity`. `DockerInfo { status: DockerStatus,
  engine_version: Option<String>, compose_version: Option<String>,
  supported: SupportedVersions { engine, compose }, message:
  Option<String> }`; `DockerStatus` is `ready | unreachable |
  unsupported_version`. `HostCapacity { cpus, memory_bytes,
  disk_free_bytes }`, each `Option<u64>` (`null` when unreadable).
  `/healthz` stays unchanged and reveals nothing.
- **Capacity (`ship-daemon::host`):** CPUs from
  `std::thread::available_parallelism`; memory from `/proc/meminfo`
  `MemTotal` on Linux (`null` elsewhere: the daemon is Linux-only);
  free disk from `statvfs(state_dir)` as `f_bavail * f_frsize`.
- **Daemon:** `Config` gains `docker: ship_docker::Runner` (default
  `Runner::docker()`). `GET /api/v1/host` runs preflight and reads capacity
  on each request and always answers `200` with `HostInfo`; Docker being
  down is data, not an error, and never stops the daemon.
- **CI:** a `docker` job on `ubuntu-latest` (Docker Engine and Compose
  preinstalled) runs the `#[ignore]`d real-Docker tests with
  `cargo test -p ship-docker -p ship-daemon -- --ignored`. The real-Docker
  tests are ignored in `make verify` so that a developer machine without
  Docker stays green; everything else runs everywhere, using fake `docker`
  programs or `DOCKER_HOST` pointing at a missing socket.

## Declared file scope

- **owned_paths:**
  - crates/ship-docker/
  - crates/ship-daemon/
  - crates/ship-core/src/api.rs
  - crates/ship-core/src/lib.rs
  - crates/ship/src/main.rs
  - .github/workflows/ci.yml
  - Cargo.lock
  - .metis/briefs/phase-0-ws-0.5.md
- **read_only_paths:** OVERVIEW.md, .metis/adr/, .metis/plans/phase-0-plan.md, Cargo.toml, crates/ship-core/ (other files), crates/ship-store/, crates/ship-net/, crates/ship/tests/, Makefile

## Definition of Done

- [ ] With Docker running, `/api/v1/host` reports `ready` and both versions (real Engine, CI Docker job).
- [ ] With the Docker socket unavailable (`DOCKER_HOST` naming a missing socket), `/api/v1/host` answers `200` with `unreachable` and an actionable message, and the daemon keeps serving afterwards.
- [ ] An Engine or Compose version outside the range reports `unsupported_version` and the message names the supported range.
- [ ] The runner has no API that accepts a shell string; a test passes an argument full of shell metacharacters (`; | & $( ) \` ' " * > <` and spaces) and gets it back byte for byte.
- [ ] The runner enforces its timeout and output cap and kills the child.
- [ ] Preflight runs only `version --format {{.Server.Version}}` and `compose version --short` (recorded by a fake `docker`), and against a real Engine the lists of containers, images, networks and volumes are identical before and after.
- [ ] `/api/v1/host` reports CPU count, and on Linux memory and free disk in the state directory.
- [ ] `ship-docker` depends only on `ship-core` among workspace crates.
- [ ] CI has a job running the real-Docker tests.
- [ ] `metis verify --post` green.

## Test plan

- `crates/ship-docker/src/runner.rs` unit tests: metacharacter argument round-trip through `/usr/bin/printf %s`; non-zero exit captured with stderr; timeout on `sleep`; output cap on `yes`; missing program is `NotFound`; environment override reaches the child.
- `crates/ship-docker/src/supported.rs` / `preflight.rs` unit tests: version parsing (suffixes, `v` prefix, garbage), range boundaries, every status outcome of the pure evaluation including messages naming the range, `unreachable` precedence.
- `crates/ship-docker/tests/fake_docker.rs`: a fake `docker` script records its argv and prints chosen versions; preflight sees exactly the two read-only commands; `ready` and `unsupported_version` end to end; `DOCKER_HOST` at a missing socket with the real CLI (or no CLI) gives `unreachable`.
- `crates/ship-docker/tests/real_docker.rs` (`#[ignore]`, CI Docker job): real Engine is `ready` with both versions in range; object lists unchanged across preflight.
- `crates/ship-daemon/tests/host.rs`: `/api/v1/host` over both listeners with a fake `ready` docker (status, versions, capacity, API version); unreachable then `/healthz` and `/api/v1/projects` still served; unsupported version names the range; `#[ignore]` real-Docker `ready`.
- `crates/ship-core/src/api.rs` unit tests: `HostInfo` JSON round-trip with the new fields, `DockerStatus` strings.
- Gate: `metis verify --post` (`make verify`).

## Out-of-scope touches

None.

## Report

Filled at implementation.
