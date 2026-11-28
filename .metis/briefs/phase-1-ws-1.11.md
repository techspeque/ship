# phase-1-ws-1.11 — Status, deployments and logs

- **Type:** feat | **Risk:** medium | **Priority:** p2
- **Coder:** claude-code/opus-5-5 | **Date:** 2026-11-28
- **Plan:** .metis/plans/phase-1-plan.md §1.11

## Goal

Observe each environment's services through Ship's labelled containers, on
request and on an interval, storing observations apart from desired state
and deployment results (ADR-0006), and serve them as `ship status`,
`ship deployments`, `ship logs PROJECT[/SERVICE] [--follow]` and an
extended `ship projects`, each `-o json` matching its contract fixture;
Docker unreachable makes every service `unknown` and changes nothing else.

## Architectural context

- ADR-0006 rules 1–4 (three facts kept apart; `healthy` only with a passing
  check; unreachable is `unknown` and blocks action), ADR-0004 rule 2 (JSON
  contract), ADR-0010 rule 2 (only a `confirmed` route's address is shown),
  ADR-0009 rule 6 (`stale_config`), OVERVIEW §3.3 (no address that does not
  exist; no secret value in any output), §2.3 (dependency rule: Docker only
  through `ship-docker`).
- Consumed unchanged: `ServiceStatus::from_observation`, `Observation`,
  `ship_docker::containers` and `ship_docker::compose::logs` (each line
  `<container>  | <docker timestamp> <text>`, stdout and stderr apart),
  the `observation` table of migration 0002, `Store::list_deployments`,
  `list_services`, `list_routes`, `get_operation`, `config::stale_services`,
  `push::answers`, `Redactor::redact`, `Queue::subscribe_closing`,
  `client::get_stream`.
- New in `ship-core`:
  - `state.rs`: `ServiceStatus::severity` order (worst first: `unknown`,
    `failed`, `unhealthy`, `starting`, `paused`, `stopped`,
    `running_unverified`, `healthy`) and `ServiceStatus::overall(iter)`:
    the worst of several, `None` for none.
  - `api.rs`: `status_path`, `deployments_path` (now also `GET`),
    `logs_path`, `logs_query(env, service, follow, tail)`, `env_query`;
    `StatusView { project: ProjectRef, environment: EnvironmentView,
    last_deployment: Option<LastDeploymentView>, services:
    Vec<ServiceStatusView> }`, `DeploymentList { deployments:
    Vec<DeploymentView> }` with `DeploymentFailureView`, `LogLineView
    { service, stream, ts, line }`; `ProjectSummary` gains `environments:
    Vec<EnvironmentSummary { id, name, desired_state, status }>`, `status`
    the overall service status, `null` with no service.
- New in `ship-store`: `observation.rs` — `NewObservation`,
  `StoredObservation`; `record_observations` (upsert, one transaction),
  `list_observations(environment)`.
- New in `ship-daemon`:
  - `observe.rs`: `observe(store, docker, environment)` — one labelled
    `containers` call for the environment; per stored service the worst of
    its containers (any release: what Docker reports now), restarts summed,
    no container is `absent`; any Docker failure is `DockerUnreachable` for
    every service. Records the observations and returns them. Never runs a
    mutating Docker command and never changes desired state or a
    deployment. `spawn_observer(…, interval)`: observes every environment
    every `Config::observe_interval` (default 30 s), stopped by
    `Running::shutdown` before the store closes.
  - `status.rs`: `status_view` (desired state from the store;
    `last_deployment` is the latest **succeeded** deployment, i.e. the one
    serving, else the latest of any result, else `null` — so a failed
    deployment leaves the running release's visible, plan §1.11 AC 4;
    services observed now; `stale_config` from `stale_services`; endpoints
    only for `confirmed` routes that `answers` now), `deployment_list`
    (newest first; `failure` from the deployment's operation when the
    result is not `succeeded`), `project_list` (each project's
    environments with their overall status, observed now).
  - `logs.rs`: the logs route. Project, environment and service are checked
    before anything streams (`404`); Docker unreachable before streaming is
    `503 docker_unavailable`. The answer is NDJSON
    (`application/x-ndjson`), one `LogLineView` per line, each line passed
    through the `Redactor`. One `compose::logs` call per service, run in a
    task that ends when the client goes away or the daemon closes. Without
    `follow` the lines are collected and sorted by `ts` (stable) before
    they are sent; with `follow` they are sent as produced. Docker's
    RFC 3339 nano timestamp is cut to milliseconds; a line without one
    gets the time it was read.
  - `http.rs`: `GET /api/v1/projects/{p}/status`, `GET …/deployments`,
    `GET …/logs`; `GET /api/v1/projects` gives `environments`.
- New in `ship`: `status.rs` — `ship status PROJECT [--env E]`,
  `ship deployments PROJECT [--env E]`, `ship logs PROJECT[/SERVICE]
  [--env E] [--follow] [--tail N]`, each with `-o json` and `--timeout`;
  `ship projects` text gains ENV, DESIRED and STATUS columns. Text status
  prints `not configured` for a service with no endpoint. Text logs print
  `SERVICE | line`, stdout lines on stdout and stderr lines on stderr; JSON
  logs print every line on stdout with its `stream`.
- The UI's exact guard of `ProjectSummary` (`web/src/lib/api.ts`) and its
  contract fixtures follow the new field, so `make web` stays green; no UI
  view changes (that is 1.13).

## Declared file scope

- **owned_paths:**
  - crates/ship-core/src/
  - crates/ship-store/src/
  - crates/ship-store/tests/observation.rs
  - crates/ship-daemon/src/
  - crates/ship-daemon/tests/status.rs
  - crates/ship-daemon/tests/web_contract.rs
  - crates/ship/src/
  - crates/ship/tests/cli_status.rs
  - crates/ship/tests/fixtures/projects.json
  - docs/contracts/cli-json.md
  - web/src/lib/api.ts
  - web/src/lib/contract/
  - Cargo.lock
  - .metis/briefs/phase-1-ws-1.11.md
- **read_only_paths:** .metis/adr/, .metis/plans/phase-1-plan.md,
  OVERVIEW.md, crates/ship/tests/fixtures/ (other than `projects.json`),
  crates/ship-store/migrations/, crates/ship-docker/, crates/ship-net/,
  crates/ship-daemon/tests/ (other than the two above), web/src/ (other
  than the two above)

## Definition of Done

- [ ] `ship status -o json`, `ship deployments -o json` and `ship logs -o json`
      match `status.json`, `status-never-deployed.json`, `deployments.json`
      and `logs.jsonl` apart from ids and timestamps (logs: exactly, the
      fake Docker's timestamps being fixed); `ship projects -o json` matches
      `projects-empty.json` and the new `projects.json`.
- [ ] A running service with no health check is `running_unverified`, with
      a passing check `healthy`, never `healthy` otherwise.
- [ ] With Docker unreachable every service is `unknown` in status and
      projects, the stored observations say unreachable, and no Docker
      command other than the read-only listing ran; desired state,
      deployments and routes are unchanged.
- [ ] A failed deployment is listed by `ship deployments` with its failure
      code, phase and message, `release_id` null and
      `previous_release_id` set, while `ship status`'s `last_deployment`
      stays the running release's succeeded deployment.
- [ ] `ship logs --follow` prints lines as they are produced (a line
      arrives before the container's log ends), stdout and stderr apart,
      and ends when the client is stopped; `PROJECT/SERVICE` narrows to one
      service; an unknown service is refused.
- [ ] Observations are recorded on request and by the interval observer.
- [ ] A secret value registered with the redactor never appears in a log
      line answered.
- [ ] An endpoint is listed only for a confirmed route that answers; none
      prints `not configured` in text.
- [ ] `metis verify --post` green.

## Test plan

- `ship-core` unit tests: `severity`/`overall` over every status; the new
  API shapes round-trip and serialise to the fixtures' key sets; query
  builders.
- `ship-store` `tests/observation.rs`: record and read back reachable and
  unreachable observations; a second record replaces the first; another
  environment's are not listed.
- `ship-daemon` `tests/status.rs`, against a fake `docker` whose `ps`,
  `container inspect` and `container logs` answers come from control files,
  with the store seeded directly (project, services, revisions, releases,
  deployments, operations, routes): status with healthy and
  no-check services, restarts and a live endpoint; never deployed;
  a failed deployment after a succeeded one (deployments list and
  `last_deployment`); Docker unreachable (every status `unknown`, stored
  observations unreachable, fake log shows only listing commands, store
  rows unchanged); the interval observer records without a request;
  logs with stdout/stderr, service filter, unknown service `404`, redacted
  secret, and follow delivering a line before the fake's log ends.
- `ship` `tests/cli_status.rs`: the built binary against an in-process
  daemon with the same kind of fake `docker`: each `-o json` output
  compared with its fixture after normalising ids and timestamps (logs and
  projects exactly where the fake fixes them), the text forms (including
  `not configured`, stderr lines on stderr), `--follow` printing the first
  line before the process is killed, usage errors exit 2.
- `ship` unit tests: renderers of status, deployments, projects and log
  lines.
- `web_contract.rs` and the web contract test: regenerated fixtures for
  `ProjectSummary`.

## Out-of-scope touches

None.

## Report

Filled at implementation.
