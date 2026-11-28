# phase-1-ws-1.14 — Recovery and invariants

- **Type:** feat | **Risk:** high | **Priority:** p2
- **Coder:** claude-code/opus-5-5 | **Date:** 2026-11-28
- **Plan:** .metis/plans/phase-1-plan.md §1.14

## Goal

A daemon that died mid-operation starts again with no Docker action: what
it left in flight is `interrupted` (operation and deployment), the
staging it left is removed and nothing else, observed state is rebuilt
from Ship's labelled containers, and `ship status` says when a deployment
was interrupted part-way and names the command that puts it right; with
invariant tests (no test secret anywhere, every listener allowed, no
unlabelled object changed) that are shown to fail on a deliberate leak.

## Architectural context

- ADR-0006 rules 4, 6, 11 (Docker unreachable is `unknown`; start
  reconciles nothing), ADR-0011 rules 1, 8 (persisted operation first;
  interrupted, never resumed), ADR-0005 rule 7 (staging), ADR-0009 rule 7
  (redaction), ADR-0003 rule 1 (listeners), ADR-0001 (labels), OVERVIEW
  §3.3. Human, 2026-11-28 (plan §1.14 task 5): in `deploying` and
  `verifying` the new containers have already replaced the old in place,
  and start takes no Docker action; the promise "previous release still
  serves" covers `building` and `preparing`.
- Consumed unchanged: `queue::recover`, `upload::clean_staging`,
  `observe::{observe, observe_all}`, `Store::{list_projects,
  list_environments, list_deployments, finish_deployment, get_operation}`,
  `DeploymentResult::Interrupted` (already allowed by the schema),
  `config::ENV_DIR`, `deploy::release_dir`, `lifecycle` redeploy (deploys
  the serving deployment's revision again).
- New / changed:
  - `ship-daemon/src/recovery.rs` (new): `recover(store, state_dir)`, run
    at start right after `queue::recover`. Every deployment with no result
    (its operation was just interrupted) is finished `interrupted`. Its
    staging is removed: a release directory it created (known by the
    operation marker below) whose release the store does not hold is
    removed whole; one whose release is recorded keeps its files
    (immutable) and loses only its env files (values on disk only for
    releases that may be brought up, ADR-0009). Release directories of
    other operations, and unmarked ones, are untouched. Runs after
    `secrets::register_all`. No Docker command.
  - `deploy::create_release_dir(state_dir, dir, operation)`: writes the
    operation id to `OPERATION_FILE` (`operation`) in the new release
    directory, so recovery can tell which operation allocated it; both
    callers (`deploy.rs`, `compose_import.rs`) pass `ctx.operation_id()`.
  - `observe::spawn_observer`: the first round runs at once, then every
    interval: observed state is rebuilt from labelled containers at start
    (read-only `ps` / `container inspect`). "Runs no Docker command at the
    next start" in AC1 is read as no command that changes anything (task 5:
    "takes no Docker action"); the test asserts every command the fake
    `docker` sees after the restart is a read (`ps`, `container inspect`),
    since task 1 requires reading Docker at start.
  - `ship-core/src/api.rs`: `InterruptionView { deployment_id,
    operation_id, release_id, phase, command }` and
    `StatusView.interruption: Option<InterruptionView>`. Present when the
    newest deployment interrupted in `deploying` or `verifying` is newer
    than every succeeded deployment of the environment. `command` is
    `ship redeploy P` (`--env E` added outside `development`) when there is
    a serving deployment to redeploy; `ship push --project P --recreate`
    when the environment is still `removed` (redeploy refuses it); else
    `ship push --project P` (redeploy refuses an environment with nothing
    deployed). An interruption in an earlier phase changed nothing in
    Docker and is not reported.
  - `ship-daemon/src/status.rs`: computes `interruption` from the history
    and the operation's `failure_phase`.
  - `ship/src/status.rs`: the text form prints, after the last deployment,
    `Interrupted: deployment D was interrupted while <phase>; Docker runs
    what it reports below. Put it right with: <command>`.
  - Fixtures: `status.json` and `status-never-deployed.json` gain
    `"interruption": null`; new `status-interrupted.json`. Web contract
    fixtures and `web/src/lib/api.ts` (`isStatusView`, `StatusView`) follow
    the new field, and the project page shows the interruption line with
    its command (the UI's `exact` guard otherwise rejects the payload).
  - Docs: README, `docs/service.md` (operator notes: recovery after a
    kill), `docs/contracts/cli-json.md` (the field; exit 3 also covers a
    push to a removed environment without `--recreate`; `192.0.2.10`
    stands for the configured publish address).

## Declared file scope

- **owned_paths:**
  - crates/ship-daemon/src/recovery.rs
  - crates/ship-daemon/src/lib.rs
  - crates/ship-daemon/src/observe.rs
  - crates/ship-daemon/src/deploy.rs
  - crates/ship-daemon/src/compose_import.rs
  - crates/ship-daemon/src/status.rs
  - crates/ship-daemon/tests/
  - crates/ship-core/src/api.rs
  - crates/ship/src/status.rs
  - crates/ship/tests/cli_status.rs
  - crates/ship/tests/fixtures/
  - web/src/lib/api.ts
  - web/src/lib/contract/
  - web/src/lib/project.ts
  - web/src/lib/project.test.ts
  - web/src/lib/ProjectPage.svelte
  - web/src/lib/contract.test.ts
  - README.md
  - docs/service.md
  - docs/contracts/cli-json.md
  - .metis/briefs/phase-1-ws-1.14.md
- **read_only_paths:** crates/ (other than the above), .metis/adr/,
  .metis/plans/phase-1-plan.md, OVERVIEW.md, web/ (other than the above)

## Definition of Done

1. After a kill (in-process: runtime dropped under a running deployment)
   in `building` or `preparing`: the operation and its deployment are
   `interrupted`, no `up` ran, the previous release's container is the one
   Docker reports, `ship status` has no `interruption`, its last
   deployment is the previous one.
2. After a kill in `deploying` or `verifying`: operation and deployment
   `interrupted`; every Docker command after the restart is a read;
   `ship status` reports the container Docker runs (the new release's),
   `last_deployment` the last good one, and `interruption` with phase,
   release and `ship redeploy app`; `ship redeploy` then succeeds and the
   interruption is gone.
3. Recovery removes the unrecorded release directory of the interrupted
   operation and the env files of a recorded one it prepared; a release
   directory of a failed (not interrupted) deployment and of the serving
   release are untouched; upload staging is still cleaned.
4. Observations are recorded at start without an operator request.
5. Invariant scenario (fake Docker, `make verify`): a stored secret and a
   registered test secret never appear in the daemon's stderr log, any
   operation event or log file, any API response of the scenario, or any
   generated Compose file / release file other than the env files; the
   scanner fails on a deliberately leaked copy.
6. The listener test fails when a listener is deliberately bound on
   0.0.0.0 in the process.
7. Real-Docker tests (ignored; CI's `docker` job): the kill in each phase
   with HTTP checks (previous serves in building/preparing; redeploy brings
   a good release back after deploying/verifying), no secret in `docker
   image history`, and an unlabelled container, network and volume created
   before the scenario unchanged after it, as is Docker's whole unlabelled
   inventory.
8. Fixtures regenerated on purpose; `metis verify --post` green.

## Test plan

- `crates/ship-daemon/tests/kill.rs` (new): fake `docker` with hang
  points per phase; one test per phase (building, preparing, deploying,
  verifying) killing the daemon in-process and restarting it; asserts DoD
  1–3, the read-only commands after restart, and redeploy recovering.
- `crates/ship-daemon/tests/recovery.rs`: observations at start; release
  staging cleaned only for interrupted operations.
- `crates/ship-daemon/tests/invariants.rs` (new): the secret scenario and
  the deliberate-leak self-test.
- `crates/ship-daemon/tests/listeners.rs`: the check as a function; a
  deliberate 0.0.0.0 listener makes it fail (serialised with the main
  test).
- `crates/ship-daemon/tests/real_recovery.rs` (new, ignored): DoD 7.
- `crates/ship-daemon/tests/status.rs`, `web_contract.rs`,
  `crates/ship/tests/cli_status.rs`: the new field, its fixture, the text
  line; web `project.test.ts`: the interruption shown as the API sends it.
- `ship-daemon/src/status.rs` unit tests: which deployment is reported as
  interrupted, and the command.

## Out-of-scope touches

None.

## Report

- Changed: `ship-daemon/src/{recovery.rs (new), lib.rs, observe.rs,
  deploy.rs, compose_import.rs, status.rs}`, `ship-core/src/api.rs`
  (`InterruptionView`, `StatusView.interruption`), `ship/src/status.rs`
  (text `Interrupted:` / `Recover:` lines), CLI fixtures
  (`status.json`, `status-never-deployed.json` gain `interruption: null`;
  new `status-interrupted.json`), web contract fixtures (same, new
  `status-interrupted.json`), `web/src/lib/{api.ts, project.ts,
  ProjectPage.svelte, project.test.ts, contract.test.ts}`, README,
  `docs/service.md`, `docs/contracts/cli-json.md`.
- Tests: new `tests/kill.rs` (kill in each of the four phases, staging
  scope), `tests/invariants.rs` (secret scan over a full scenario in a
  child process with the real logger, plus deliberate-leak self-test),
  `tests/real_recovery.rs` (ignored; real Engine: kill per phase, image
  history, unlabelled inventory); `listeners.rs` gained a deliberate
  0.0.0.0 / stray-socket test; `recovery.rs` gained start observation.
- `metis verify --post`: green (run 6). Runs 1–5 failed on fmt, one
  clippy lint, test setup (secret before project; fake read only two
  Compose files) and once on the pre-existing
  `push::tests::only_an_answering_address_answers` (port reuse flake;
  untouched code, passed on rerun).
- Not run here: the `real_*` tests (no Docker access in this session);
  CI's docker job runs them.
- Decisions: AC1's "runs no Docker command at the next start" taken as
  no command that changes anything; the start observation (task 1) runs
  `ps` / `container inspect`, and the tests assert only those appear.
  The recovery command is `ship push --project P` when nothing ever
  succeeded and `ship push --project P --recreate` for a removed
  environment, since `ship redeploy` refuses both.
