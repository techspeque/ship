# phase-1-ws-1.6 — Docker and Compose adapter

- **Type:** feat | **Risk:** high | **Priority:** p2
- **Coder:** claude-code/opus-5-5 | **Date:** 2026-11-27
- **Plan:** .metis/plans/phase-1-plan.md §1.6

## Goal

Give `ship-docker` the Compose and Docker commands Phase 1 deploys with —
`compose config`, `build`, `up`, `stop`, `start`, `restart`, `down` without
volumes, `ps`, container `inspect` and `logs` — each built from structured
argv, selecting and touching only objects carrying Ship's labels, labelling
everything it creates, parsing inspect into `Observation`, streaming output
line by line, keeping `compose config` output out of every log, and making
volume removal and a shell impossible to express.

## Architectural context

- ADR-0001 rules 2, 4, 5; ADR-0006 rules 2, 4; ADR-0008 rule 7;
  OVERVIEW §3.3 (structured argv, never `down -v`, only labelled objects,
  Docker unreachable is `unknown`), §6 Docker labels, §2.3 (`ship-docker`
  depends only on `ship-core` among workspace crates).
- Consumed unchanged from `ship-core`: `ProjectId`, `EnvironmentId`,
  `ServiceId`, `ReleaseId`, `compose_project_name`, `Observation`,
  `ContainerState`, `HealthCheck`, `SecretValue`.
- Existing in `ship-docker`: `Runner` (structured argv, no shell, process
  group, timeout, bounded output) and `preflight`. `Runner::run` becomes
  `pub(crate)`, so nothing outside the crate can hand Docker an argv the
  adapter did not build (it is called nowhere outside the crate).
- New in `ship-docker`:
  - `labels.rs`: `PROJECT_LABEL` … `RELEASE_LABEL` (`dev.ship.project`,
    `dev.ship.environment`, `dev.ship.service`, `dev.ship.release`);
    `ShipLabels` { project, environment, service: `Option<ServiceId>`,
    release } (no service = shared by the environment, value empty);
    parsing from a Docker label map; `Selector` (always the environment
    label, optionally the service) rendered as `--filter label=…`.
  - `argv.rs`: `Argv`, the only argument vector the adapter executes. Its
    builders are the only constructors; each passes a deny check (no
    `-v`, `--volumes`, `--rmi`, `volume`, `prune`, `rm`, `exec`, `run`,
    `sh`, `-c` before the `--` separator). Service names are checked
    against Compose's name rule and passed after `--`; container and
    network ids must be hex; file paths and the project directory must be
    absolute. `ComposeProject` (name from `compose_project_name(env)`)
    and `ComposeFiles` { directory, files }.
  - `compose.rs`: `config(…) -> ComposeConfig` (`--format json`; `Debug`
    redacted, no `Display`, failure stderr held as `SecretValue`),
    `labels_override(&ComposeConfig, &DeployLabels) -> String` (a JSON
    Compose override — valid YAML — adding the four labels to every
    service's containers and built image, and to every non-external
    network and volume), `build`, `up` (`--detach --no-build`, optional
    `--remove-orphans`), `stop`, `start`, `restart`, `down` (never `-v`,
    `--volumes` or `--rmi`), `ps` (ids), `logs`. Name-only commands
    (`stop`, `start`, `restart`, `down`, `ps`, `logs`) use `-p` alone.
    Every mutating command first checks that every container and network
    in the Compose project carries Ship's labels for that environment, and
    refuses with `DockerError::Foreign` otherwise.
  - `inspect.rs`: `containers(runner, &Selector) ->
    Vec<ContainerObservation>` (`docker ps --filter label=…` then
    `container inspect`, re-checking the labels on what comes back);
    `parse_inspect` into `ContainerObservation` { id, name,
    compose_service, labels, observation, restart_count }, covering
    status, exit code, health and `OOMKilled`.
  - `stream.rs`: `Stream` (stdout | stderr), `Line`, and the streaming
    run used by `build`, `up` and `logs`: each line, cut at 64 KiB, goes
    to a sink as it arrives; the process group is killed on timeout or
    drop.
  - `error.rs`: `DockerError` — `Unreachable` (CLI missing, socket
    missing or refused, permission denied) distinct from `Failed` (ran and
    exited non-zero), plus `Timeout`, `OutputTooLarge`, `Io`, `Malformed`,
    `Foreign`, `InvalidArgument`, `ConfigFailed`.
- New dependencies of `ship-docker`: `serde` and `serde_json` (already in
  the workspace lock, MIT or Apache-2.0).

## Declared file scope

- **owned_paths:**
  - crates/ship-docker/
  - Cargo.lock
  - .metis/briefs/phase-1-ws-1.6.md
- **read_only_paths:** .metis/adr/, .metis/plans/phase-1-plan.md,
  OVERVIEW.md, crates/ship-core/, crates/ship-daemon/,
  .github/workflows/ci.yml, deny.toml

## Definition of Done

- [ ] No adapter function can produce an argv that removes a volume or
      runs a shell: a test drives every builder with hostile inputs
      (service names `-v`, `--volumes`, `volume`, `;sh`, relative paths)
      and checks each is refused or yields an argv free of the denied
      tokens; `Argv` has no public constructor besides the builders.
- [ ] An object without Ship's labels is never returned, stopped or
      removed: against real Docker, a container in the Compose project
      without the labels makes `stop`, `restart` and `down` refuse and is
      still there afterwards, and `containers` never returns it.
- [ ] Every container, network and volume created by `up` with the
      override carries the four labels (real Docker), and `down` leaves
      the named volume in place.
- [ ] `parse_inspect` gives the right `Observation` for running, paused,
      restarting, exited (with code), dead, created, health starting,
      healthy, unhealthy, and the OOM flag; restart count is read.
- [ ] Build and container output reach the sink line by line with stdout
      and stderr told apart (real Docker and a fake `docker`).
- [ ] Docker unreachable (missing CLI, missing socket) is
      `DockerError::Unreachable`; a command that ran and failed is
      `DockerError::Failed`.
- [ ] `ComposeConfig` and a failed `config` never print the interpolated
      content in `Debug` or `Display`.
- [ ] `ship-docker` depends only on `ship-core` among workspace crates.
- [ ] `metis verify --post` green.

## Test plan

- Unit (`argv.rs`): exact argv of every builder; deny check over every
  builder with hostile names, ids and paths; `Argv` refused when a
  denied token appears before `--`.
- Unit (`labels.rs`): label pairs, parsing (missing, malformed, wrong
  environment), selector filters.
- Unit (`inspect.rs`): inspect JSON fixtures for each state, health and
  OOM; unlabelled entries dropped.
- Unit (`compose.rs`): override from a canonical config fixture (build
  and image services, external network and volume skipped, missing
  service mapping refused); `ComposeConfig` `Debug` redacted.
- Unit (`stream.rs`): long lines cut, stdout/stderr apart, timeout kills.
- Unit (`error.rs`): unreachable stderr patterns classified.
- `tests/fake_docker.rs` (extended): argv recorded by a fake `docker` for
  `stop`, `down`, `logs`; a fake that fails with a daemon error is
  `Failed`, one that cannot connect is `Unreachable`; real CLI with a
  missing socket is `Unreachable`.
- `tests/real_docker.rs` (ignored; CI `docker` job): a two-service
  Compose project (one built, health check, named volume, custom network)
  through config, override, build (lines streamed), up, containers
  (labels and observation), stop (exit under desired stopped is
  `stopped`), start, restart, logs (stdout and stderr apart), down
  (volume kept); a foreign container in the project blocks `stop`,
  `restart`, `down` and is never returned.

## Out-of-scope touches

None.

## Report

Filled at implementation.
