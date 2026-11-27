# phase-1-ws-1.2 — Store for Phase 1

- **Type:** feat | **Risk:** medium | **Priority:** p2
- **Coder:** claude-code/opus-5-5 | **Date:** 2026-11-27
- **Plan:** .metis/plans/phase-1-plan.md §1.2

## Goal

Bring the SQLite store to what Phase 1 needs: tables for route, config
value, secret metadata and observation; repositories for revision, release
and deployment; operation phase, failure and log-cursor updates with the
transition rules enforced in the store; desired-state updates; an
append-only audit log; and an atomic get-or-create of a project with its
`development` environment.

## Architectural context

- OVERVIEW §2.2 data model; ADR-0006 (desired state separate from
  observation), ADR-0007 (revision and release immutable, several
  deployments per release, a failed build has no release), ADR-0009 (secret
  values never in the database), ADR-0010 (route record, `port` provider
  only in Phase 1), ADR-0011 (operation phases, typed failures naming the
  phase).
- `docs/contracts/cli-json.md` and `crates/ship/tests/fixtures/deployments.json`:
  a deployment carries `revision_id`, a nullable `release_id` (null when
  the build failed) and a failure with `code`, `phase` and `message`. The
  Phase 0 `deployment` table has `release_id NOT NULL` and no
  `revision_id`, and the operation row has no failure phase, so migration
  0002 rebuilds `deployment` (filling `revision_id` from the release) and
  adds `operation.failure_phase`.
- `ship_core::state`: `DesiredState::transition`,
  `OperationPhase::transition` / `status()`; `ship_core::ErrorCode` for
  failure codes. New in `ship-core`: `RouteId` (prefix `rte`),
  `RevisionKind` (`snapshot` | `git` | `image`), `DeploymentResult`
  (`succeeded` | `failed` | `interrupted`).
- Migration 0002 (`crates/ship-store/migrations/0002_phase1.sql`):
  - `route`: id, environment, service, provider (`port` only), address,
    status (`pending` | `confirmed` | `failed`), created_at, updated_at.
  - `config_value`: scope (`project` | `environment` | `service`) with one
    matching foreign-key column, key, value, updated_at; unique per scope
    target and key.
  - `secret`: scope with one matching foreign-key column, name,
    backend_ref, updated_at. No other column.
  - `observation`: one row per service: reachable, container state, exit
    code, health, OOM flag, restarts, observed_at.
  - Triggers: operation phase moves only along `OperationPhase`'s rules,
    status always equals the phase's status, failure fields set exactly on
    `failed` / `interrupted` and never changed after, log cursor never
    decreases; desired state moves only along `DesiredState`'s rules;
    deployment's identity fields never change, `release_id` and `result`
    are set at most once, its operation belongs to the same environment;
    audit events are never updated or deleted. Revision and release
    immutability triggers are kept.
- Store API (`ship-store`), all on `Store`:
  - `get_or_create_project(&Slug) -> (Project, Environment)` in one
    `BEGIN IMMEDIATE` transaction.
  - `set_desired_state(EnvironmentId, DesiredState)`; same state is a no-op.
  - `create_revision`, `get_revision`, `list_revisions`;
    `create_release`, `get_release`, `list_releases`;
    `create_deployment`, `get_deployment`, `list_deployments` (newest
    first), `set_deployment_release`, `finish_deployment`.
  - `advance_operation(id, phase)` (not to `failed` / `interrupted`),
    `fail_operation(id, terminal, &OperationFailure)` (records code,
    message and the phase it failed in), `set_log_cursor(id, u64)`,
    `unfinished_operations()` (status `queued` or `running`).
  - `append_audit_event`, `list_audit_events`.
  - Illegal transitions are `StoreError::IllegalTransition { entity }`
    (new variant), whether caught in Rust or by a trigger.
- Repositories for route, config value, secret and observation belong to
  later slices (1.9–1.11); this slice adds only their tables.

## Declared file scope

- **owned_paths:**
  - crates/ship-store/
  - crates/ship-core/src/id.rs
  - crates/ship-core/src/model.rs
  - crates/ship-core/src/lib.rs
  - Cargo.lock
  - .metis/briefs/phase-1-ws-1.2.md
- **read_only_paths:** .metis/adr/, .metis/plans/phase-1-plan.md,
  OVERVIEW.md, docs/contracts/cli-json.md, crates/ship/tests/fixtures/,
  crates/ship-core/src/state.rs, crates/ship-core/src/error.rs,
  crates/ship-daemon/

## Definition of Done

- [ ] A Phase 0 (version 1) database with data migrates in place to
      version 2, with a backup at version 1, keeps every row (deployment
      `revision_id` filled from its release), and reopens cleanly.
- [ ] The `secret` table's columns are exactly scope, its foreign-key
      columns, name, backend_ref and updated_at.
- [ ] An illegal operation phase or status transition, an update after a
      terminal phase, a decreasing log cursor, and an illegal desired-state
      change are each rejected by the store API and, through raw SQL, by
      the schema.
- [ ] Concurrent `get_or_create_project` calls for one slug yield one
      project and one `development` environment.
- [ ] A deployment without an operation, or with an operation of another
      environment, is rejected by the schema; revision and release updates
      are still rejected.
- [ ] Every new repository function has a test against a real SQLite file.
- [ ] `metis verify --post` green.

## Test plan

- `crates/ship-store/tests/store.rs` (real SQLite files in temp dirs):
  - migration from a hand-built version 1 database with a project,
    environment, operation, revision, release and deployment: backup
    exists and is at version 1, rows survive, `revision_id` filled,
    reopen takes no second backup; table list includes the four new ones.
  - `secret` column list via `PRAGMA table_info`.
  - revision, release, deployment create/get/list round trips; deployment
    with a failed build (no release), `set_deployment_release` once,
    `finish_deployment` once; second attempts rejected.
  - raw SQL: deployment with no / unknown / other-environment operation
    rejected; revision and release update rejected (kept).
  - operation: advance through phases, skip phases, backwards / same /
    from terminal rejected; status always matches phase; `fail_operation`
    records code, message and failure phase; `failed` via `advance`
    rejected; raw SQL bad phase, mismatched status, and changed failure
    rejected; log cursor up accepted, down rejected; `unfinished_operations`
    returns exactly queued and running ones.
  - desired state: legal changes, no-op on same state, `removed` final,
    raw SQL illegal change rejected.
  - get-or-create: idempotent; 16 concurrent calls on one slug → one
    project, one environment, all results equal.
  - audit: append and list in order; raw update and delete rejected.
- `ship-core`: `RouteId` round trip and distinct prefix;
  `RevisionKind`, `DeploymentResult` stable strings.

## Out-of-scope touches

None.

## Report

Filled at implementation.
