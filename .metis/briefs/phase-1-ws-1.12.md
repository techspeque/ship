# phase-1-ws-1.12 — Lifecycle commands

- **Type:** feat | **Risk:** high | **Priority:** p2
- **Coder:** claude-code/opus-5-5 | **Date:** 2026-11-28
- **Plan:** .metis/plans/phase-1-plan.md §1.12

## Goal

`ship start`, `stop` and `restart` for a project or one service, and
`ship redeploy` and `remove` for a project, each a durable operation on its
environment's lock and an audit event; a per-service desired state (new
migration) from which status derives, so a service stopped on purpose is
never `failed`; a removed environment keeps its volumes and its history and
comes back only through `ship push --recreate` (Human, 2026-11-28).

## Architectural context

- ADR-0006 rules 1, 2, 6, 10 (desired / last deployment / observed kept
  apart; stop is not failure; a Ship stop is never undone by Ship; restart
  is not redeploy), ADR-0011 (persisted operation before any side effect,
  one mutation per environment at a time), ADR-0007 (history), ADR-0001
  (only Ship-labelled objects; never `down -v`), OVERVIEW §3.3, §6.
- Consumed unchanged: `ship_docker::compose::{stop, start, restart, down,
  config}` (each label-checked by `ensure_only_ours`), `Queue::submit`,
  `http::accept`, `deploy::run`, `release_files`, `Context::{enter, log}`,
  `append_audit_event`, `replace_routes`, `crate::settings::submit` (client).
- `ship-core`:
  - `state.rs`: `DesiredState::effective(environment, service)`: the
    environment's when it is `stopped` or `removed`, else the service's.
  - `api.rs`: kinds `start`, `stop`, `restart`, `redeploy`, `remove`;
    `lifecycle_path(project, action)` =
    `/api/v1/projects/{p}/{action}`, `env`/`service` in the query
    (`config_query`); `DeployRequest.recreate` and `PushOptions.recreate`
    (`recreate=true` in the push query; `push_query` takes the options);
    `RECREATE_REQUIRED` — the fixed start of the failure message of a push
    to a removed environment, which the client tells apart from a Compose
    confirmation; `DeploymentView.action` and `LastDeploymentView.action`
    (`deploy` | `remove`); `ServiceStatusView.desired_state`.
- `ship-store`:
  - migration `0005_lifecycle.sql`: `service.desired_state`
    (`running` | `stopped`, default `running`); `deployment.action`
    (`deploy` | `remove`, default `deploy`) and `deployment.options`
    (JSON `{file, port}` of the deploy, NULL for a removal or a row from
    before); the deployment triggers rebuilt so a `remove` row has no
    release and may succeed without one, and `action`/`options` are
    identity fields; the environment trigger allows `removed → running`
    only when a `deploy` deployment of that environment succeeded after
    its latest removal, every other move out of `removed` refused.
  - `Service.desired_state`; `set_service_desired_state(service, to)`
    (`running ↔ stopped` only); `set_services_running(environment)`.
  - `Deployment.action`, `Deployment.options`; `create_deployment_with
    (new, options)`; `record_removal(environment, operation, revision,
    previous_release)` (opened and succeeded in one transaction);
    `recreate_environment(environment, deployment)`: `removed → running`
    through that succeeded deployment, refused otherwise.
- `ship-daemon`:
  - `lifecycle.rs`: the five actions. Each re-reads the environment under
    its lock. `start P`: environment and every service `running`,
    `compose start`. `start P/S`: service `running`, `compose start S`;
    refused while the environment is `stopped` (start the project).
    `stop P`: environment `stopped`, `compose stop`. `stop P/S`: service
    `stopped`, `compose stop S`. `restart P`: `compose restart` of the
    services desired `running`; `restart P/S`: refused when the service or
    the environment is `stopped`. None of the three changes the release.
    `redeploy P`: `deploy::run` of the serving deployment's revision with
    its recorded `options`. `remove P`: `compose down` (no volumes), routes
    emptied, a `remove` deployment recorded, desired `removed`; an already
    removed environment succeeds changing nothing. On a removed
    environment `start`, `stop`, `restart` and `redeploy` fail
    `conflict` saying to push with `--recreate`. Desired state is recorded
    before Docker is asked to stop and after Docker started; a Docker
    failure fails the operation with the phase. One audit event per
    action (`actor` operator, target the environment or service id).
  - `http.rs`: `POST /api/v1/projects/{p}/{start|stop|restart|redeploy|
    remove}`; `service` on `redeploy`/`remove` is `400` with the reason
    before any operation.
  - `deploy.rs`: the environment is re-read in the operation. A removed
    environment without `recreate` fails `confirmation_required` in
    `validating` before anything is recorded, naming the named volumes
    that would be reattached (from `compose config` of the last release,
    best effort); with it, the deployment runs with no previous release
    (nothing serves after a removal) and, once succeeded, the environment
    becomes `running` through `recreate_environment`. A succeeded
    deployment sets a `stopped` environment and every service `running`
    (a deployment asks for everything running). A failure on a `stopped`
    environment stops the previous release again after bringing it back,
    so a failed push never starts what the operator stopped.
  - `observe.rs`, `status.rs`, `push.rs`: status from
    `DesiredState::effective`; deployments and status show `action`;
    status shows each service's own desired state.
- `ship`: `lifecycle.rs` — `ship start|stop|restart PROJECT[/SERVICE]`,
  `ship redeploy|remove PROJECT` (a `/SERVICE` is refused, exit 2, before
  anything is sent), `--env`, `--timeout`, `-o json` (the final
  `OperationView`); events printed as `ship push` does; exit 0 succeeded,
  1 failed. `remove` asks on a terminal, naming that containers and
  networks go and volumes and history stay; with no terminal it needs
  `--confirm`, else exits 3 and sends nothing. `ship push --recreate`; on
  a terminal a push to a removed environment prints the volumes and asks,
  then deploys the stored revision with `recreate`; without a terminal it
  exits 3 with a hint naming `--recreate`.

## Declared file scope

- **owned_paths:**
  - crates/ship-core/src/
  - crates/ship-store/src/
  - crates/ship-store/migrations/0005_lifecycle.sql
  - crates/ship-store/tests/lifecycle.rs
  - crates/ship-daemon/src/
  - crates/ship-daemon/tests/lifecycle.rs
  - crates/ship/src/
  - crates/ship/tests/cli_lifecycle.rs
  - crates/ship/tests/fixtures/status.json
  - crates/ship/tests/fixtures/deployments.json
  - docs/contracts/cli-json.md
  - Cargo.lock
  - .metis/briefs/phase-1-ws-1.12.md
  - crates/ship-store/tests/deploy.rs — out-of-scope touch, below
  - crates/ship-store/tests/phase1.rs — out-of-scope touch, below
- **read_only_paths:** .metis/adr/, .metis/plans/phase-1-plan.md,
  OVERVIEW.md, crates/ship-store/migrations/ (other than 0005),
  crates/ship-docker/, crates/ship-net/, crates/ship-source/, web/,
  every test file not listed above

## Definition of Done

- [ ] `ship stop P` then `ship status P`: environment `stopped`, every
      service `stopped` (not `failed`, exit 137/143 included); across a
      daemon restart the same, and no `start`/`up` is run by the daemon.
- [ ] `ship stop P/S` on a two-service environment: S `stopped` with
      desired `stopped`, the other unchanged, across a daemon restart;
      `ship start P/S` runs it again and it is `running` desired.
- [ ] `restart` runs `compose restart` and leaves the release and the
      deployment history unchanged; `redeploy` after a config change
      records a new deployment of the same revision and clears
      `stale_config`.
- [ ] `remove` runs `compose down` with no volume flag, keeps the
      deployments, records a `remove` deployment, empties routes, sets
      `removed`; no argv anywhere removes a volume.
- [ ] `redeploy P/S` and `remove P/S` are refused with the reason (CLI
      exit 2 sending nothing; API `400` creating no operation).
- [ ] `start`/`stop`/`restart`/`redeploy` on a removed environment fail
      naming `--recreate` and run no Docker command.
- [ ] After `remove`, a push without `--recreate` and no terminal exits 3
      and records no deployment; with `--recreate` it deploys, the
      environment is `running`, and `ship deployments` lists the old
      deployments, the removal and the new one.
- [ ] The store refuses `removed → running` except through
      `recreate_environment` after a succeeded deploy, and the trigger
      refuses it in raw SQL.
- [ ] A lifecycle request while another operation holds the environment
      waits its turn (runs after it, never at the same time).
- [ ] Each action appends one audit event.
- [ ] A Phase 1 database (0004) migrates to 0005 with a backup; services
      read `running`, deployments `deploy`.
- [ ] `status.json` and `deployments.json` carry the new fields and still
      match; `cli-json.md` documents the commands, fields and exit codes.
- [ ] `metis verify --post` green.

## Test plan

- `ship-core` unit: `effective` over every pair; `push_query` /
  `parse_push_query` round trip with `recreate`; lifecycle paths.
- `ship-store` `tests/lifecycle.rs` (real SQLite): service desired state
  set/read and illegal values; `record_removal` row shape and history
  order; `recreate_environment` refused before a deploy succeeded after
  the removal, accepted after, and the raw-SQL trigger refusing
  `removed → running`/`stopped` otherwise; migration of a 0004 database.
- `ship-daemon` `tests/lifecycle.rs`, against a fake `docker` recording
  every argv and playing two services from control files: each action's
  argv and desired states; stop → status `stopped` with exit 143 →
  daemon restarted → still stopped, no start argv; per-service stop/start;
  restart keeps the release; redeploy after `vars set` clears
  `stale_config`; remove (down argv has no `-v`/`--volumes`, history kept,
  routes empty); `/SERVICE` refusals with no operation; actions on a
  removed environment; push to a removed environment without and with
  `recreate`; lifecycle queued behind a held environment lock; audit
  events.
- `ship` `tests/cli_lifecycle.rs`: the built binary against an in-process
  daemon with the fake `docker`: `stop`/`start` exit codes and text,
  `redeploy P/S` exit 2, `remove` without a terminal and `--confirm` exit
  3 sending nothing, `remove --confirm`, `push` to the removed project
  exit 3 then `push --recreate` exit 0 and `deployments -o json` listing
  the removal.
- Unit tests of the renderers (deployments `ACTION` column, status
  desired column) and the CLI parsing.

## Out-of-scope touches

- `crates/ship-store/tests/deploy.rs` (3 lines) and
  `crates/ship-store/tests/phase1.rs` (1 line): the literal schema version
  `4` in `assert_eq!(…, 4)` becomes `5`. Why: both tests pin
  `LATEST_VERSION` to 4, so any new migration fails them; migration 0005 is
  what the plan asks for (§1.12 task 2). No assertion changes otherwise.

## Report

Filled at implementation.
