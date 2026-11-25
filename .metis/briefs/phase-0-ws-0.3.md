# phase-0-ws-0.3 — SQLite store and migrations

- **Type:** feat | **Risk:** medium | **Priority:** p2
- **Coder:** claude-code/opus-5-5 | **Date:** 2026-11-25
- **Plan:** .metis/plans/phase-0-plan.md §0.3

## Goal

`ship-store` opens one SQLite database in WAL mode inside the state
directory, applies in-repo migrations (backing up an existing database first
and refusing a database newer than the binary), and provides repositories
for project, environment, service and operation (with gapless operation
events), with revision and release rows immutable in the schema.

## Architectural context

- OVERVIEW §2.2 data model; §2.3 `ship-store` is a leaf: depends only on
  `ship-core`; §3.1 SQLx, WAL, migrations in-repo; §8 "Migrations back up
  the database first".
- ADR-0002 rules 1, 3: SQLx (SQLite, WAL); schema changes only through
  in-repo migrations; domain mutations in transactions.
- ADR-0006 rule 1/5: desired state `running | stopped | removed`; recovery
  policy per environment, `observe_only` (default) or `repair_safe_drift`.
- ADR-0007 rule 1: revisions and releases are never edited.
- ADR-0011 rules 1, 2, 5: idempotency key, a replayed key returns the
  original operation; phases; per-operation event sequence numbers.
- Consumes from `ship-core` (ws-0.2): the seven id types (`from_parts`,
  `Display`/`FromStr`), `Slug`, `DesiredState`, `OperationPhase`,
  `OperationStatus`, `Timestamp`, `compose_project_name`.
- New external crates: `sqlx` 0.8 (`sqlite`, `runtime-tokio`, no macros),
  `getrandom` (id entropy). Dev: `tokio` (test runtime), `tempfile`.

Design decisions:
- **File:** `<state_dir>/ship.db`. `Store::open(state_dir)` requires the
  directory to exist. Connection options: WAL, `foreign_keys = ON`,
  `synchronous = NORMAL`, busy timeout 5 s.
- **Migration runner:** own, in `ship-store`: an ordered list of
  `(version, sql)` with SQL in `crates/ship-store/migrations/NNNN_*.sql`
  (`include_str!`). Schema version is `PRAGMA user_version`. Each migration
  runs in one transaction together with its `user_version` bump.
- **Newer schema:** before anything is written, the version is read through
  a read-only connection; if it exceeds the binary's latest,
  `StoreError::SchemaTooNew { found, supported }` and nothing is changed
  (no WAL switch, no migration, no backup).
- **Backup:** if the database file existed and was non-empty before open
  and at least one migration is pending, `VACUUM INTO
  <state_dir>/ship.db.backup-v<from>-<unix_ms>` runs before the first
  migration. A fresh database gets no backup.
- **Schema (migration 0001):** STRICT tables `project`, `environment`,
  `service`, `revision`, `release`, `deployment`, `operation`,
  `operation_event`, `audit_event`. Ids are their `ship-core` string form,
  times are unix milliseconds. Enumerated columns carry `CHECK (... IN
  (...))`. `BEFORE UPDATE` triggers on `revision` and `release` raise
  `ABORT`. The Compose project name is not stored; it is derived from the
  environment id on read.
- **Domain additions in `ship-core`:** `ServiceKind` (`web | worker | job |
  compose`) and `RecoveryPolicy` (`observe_only | repair_safe_drift`) as
  `string_enum!`s in a new `model.rs`; pure, no I/O.
- **Repositories:** `Store` methods. Projects: create(slug), get, list.
  Environments: create(project, name: Slug, desired state, recovery policy),
  get, list(project). Services: create(environment, name, kind, port), get,
  list(environment). Operations: create(idempotency key, kind,
  environment?) — inserts with phase `queued`, or returns the existing
  operation for a replayed key together with `created = false`; get;
  append_event(operation, type, phase, message) → assigned sequence.
- **Gapless sequences:** one `INSERT … SELECT COALESCE(MAX(sequence), 0) + 1
  … RETURNING sequence` statement per append; SQLite's single writer makes
  it atomic. Primary key `(operation_id, sequence)`.
- **Ids and times:** generated in the store from the system clock and
  `getrandom` entropy.
- **Errors:** `StoreError` — `SchemaTooNew`, `NotFound`, `Conflict` (unique
  or foreign-key violation), `Corrupt` (a stored value fails to parse),
  `Io`, `Database`.
- Service name validation (Compose names) is Phase 1's importer; the schema
  requires 1–63 characters.

## Declared file scope

- **owned_paths:**
  - crates/ship-store/
  - crates/ship-core/src/model.rs
  - crates/ship-core/src/lib.rs
  - Cargo.lock
  - .metis/briefs/phase-0-ws-0.3.md
- **read_only_paths:** OVERVIEW.md, .metis/adr/, .metis/plans/phase-0-plan.md, Cargo.toml, crates/ship-core/ (other files), crates/ship/tests/dependency_rule.rs

## Definition of Done

- [ ] `Store::open` on an empty state directory creates `ship.db` in WAL mode at the latest schema version; closing and reopening succeeds and applies nothing.
- [ ] Migration 0001 creates the nine tables of §0.3.
- [ ] `UPDATE` on a `revision` or `release` row fails; insert still works.
- [ ] Project, environment and service create/get/list round-trip their fields; a duplicate slug / name is `Conflict`; an unknown id is `NotFound`.
- [ ] Operation create is idempotent on its key; get returns it; append_event under concurrent appends yields sequences exactly `1..=n` for that operation.
- [ ] Opening a database whose `user_version` exceeds the latest returns `SchemaTooNew` and leaves the file bytes, its version and the directory listing unchanged.
- [ ] Opening an existing database with a pending migration leaves a backup file whose schema version is the pre-migration version; a fresh open leaves none.
- [ ] `ship-store` depends only on `ship-core` among workspace crates (existing dependency-rule test).
- [ ] `metis verify --post` green.

## Test plan

- `crates/ship-store/tests/store.rs` (tokio, `tempfile` state dirs):
  - `fresh_state_dir_migrates_and_reopens` — version, `journal_mode = wal`, table set; reopen applies nothing, no backup.
  - `revision_and_release_reject_update` — insert ok, UPDATE errors.
  - `project_environment_service_roundtrip`, `duplicate_slug_conflicts`, `unknown_id_not_found`, `foreign_key_enforced`.
  - `operation_create_is_idempotent`, `concurrent_appends_are_gapless` (many tasks over the pool, plus a second operation interleaved).
  - `newer_schema_is_refused_and_untouched`.
- Unit tests in `crates/ship-store/src/migrate.rs`: `backup_before_migrating_existing_database` (with a synthetic second migration), `no_backup_on_fresh_database`, migration versions strictly increasing from 1.
- `crates/ship-core/src/model.rs`: string forms stable and round-trip.
- Gate: `metis verify --post` (`make verify`).

## Out-of-scope touches

None.

## Report

Filled at implementation.
