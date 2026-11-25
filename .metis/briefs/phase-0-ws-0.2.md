# phase-0-ws-0.2 — Domain types and state machines

- **Type:** feat | **Risk:** high | **Priority:** p2
- **Coder:** claude-code/opus-5-5 | **Date:** 2026-11-25
- **Plan:** .metis/plans/phase-0-plan.md §0.2

## Goal

`ship-core` holds the typed ids, slug and Compose project name rules, the
desired-state / service-status / operation-phase state machines, the typed
API error, the `/api/v1/host` and `/api/v1/projects` wire types and a
redacting secret type, all pure (no I/O) and shared by client and daemon.

## Architectural context

- OVERVIEW §2.2 entities; §2.3 `ship-core` is the root crate, depends on no
  workspace crate, performs no I/O; §6 error shape
  `{ "code", "message", "details", "operation_id" }` and `/api/v1`.
- ADR-0001 rule 4: Compose project name derives from the immutable
  environment id, never from user input.
- ADR-0006 rules 1, 2, 4: desired state `running | stopped | removed`, set
  only by operator action; service status `healthy | running_unverified |
  starting | unhealthy | failed | stopped | unknown`; `healthy` requires a
  passing health check; Docker unreachable is `unknown`.
- ADR-0011 rule 2: phases in order `queued, uploading, validating, building,
  preparing, deploying, verifying`, then terminal `succeeded | failed |
  interrupted`; skipping allowed; rule 7 typed failures with stable codes.
- ADR-0009 rule 7 / OVERVIEW §3.3: no secret value in any output.
- ADR-0004 rule 4: JSON shapes provisional until Phase 5.
- First external crates: `serde` (derive) and `serde_json` (error `details`).
  Both are pure; neither performs I/O on its own.

Design decisions:
- **Ids:** `<prefix>_<26 lowercase Crockford base32 chars>` encoding a
  128-bit ULID-layout value (48-bit ms timestamp + 80-bit entropy).
  Prefixes: `prj`, `env`, `svc`, `rev`, `rel`, `dep`, `op`. One type per
  entity; fields private; no mutators. Built from caller-supplied
  `(timestamp_ms, entropy)` so `ship-core` reads no clock or RNG; generation
  with real time/entropy belongs to the caller (store/daemon). Parsing is
  strict: exact prefix, exact length, lowercase canonical alphabet only, no
  128-bit overflow. Serde as the string form.
- **Slug:** 1–63 chars of `[a-z0-9-]`, starts with a letter, does not end
  with `-`, no `--`.
- **Compose project name:** `ship_<environment id string>` e.g.
  `ship_env_01…`; built only from `EnvironmentId`, so it contains no user
  input and satisfies Compose's `[a-z0-9][a-z0-9_-]*`.
- **Desired state:** legal transitions `running↔stopped`,
  `running→removed`, `stopped→removed`; `removed` is terminal; a
  same-state "transition" is rejected (callers treat it as a no-op before
  asking).
- **Service status:** observed, not commanded: Docker is authoritative, so
  any status may follow any other. The rule enforced is evidence: status is
  derived from an observation (`docker reachable?`, container state, health
  check result); `healthy` only from a passing check, a running container
  without a check is `running_unverified`, Docker unreachable is `unknown`.
- **Operation phase:** forward-only through the ordered list, skipping
  allowed; any non-terminal phase may go to `succeeded`, `failed` or
  `interrupted`; terminal phases have no exits; no self-transitions.
  `OperationStatus` (`queued | running | succeeded | failed | interrupted`)
  derived from phase for the data model's `status` column.
- **Error:** `ErrorCode` enum, snake_case stable strings; `ApiError
  { code, message, details: serde_json::Value (default `{}`), operation_id:
  Option<OperationId> }`, `Display` + `std::error::Error`.
- **API:** `API_VERSION = "v1"`, `API_PREFIX = "/api/v1"`,
  `HostInfo { api_version, version, os, arch }`,
  `ProjectList { projects: Vec<ProjectSummary { id, slug, created_at }> }`.
  `Timestamp` = UTC unix milliseconds, serialised as RFC 3339
  `YYYY-MM-DDTHH:MM:SS.mmmZ`, formatted and parsed in-crate (no clock read).
- **Secret:** `Secret<String>`-style `SecretValue`: `Debug`/`Display` print
  `[redacted]`, no `Serialize`, no `Clone`-to-string path except an
  explicit `expose()`; zeroing on drop is not claimed.

## Declared file scope

- **owned_paths:**
  - crates/ship-core/
  - Cargo.lock
  - .metis/briefs/phase-0-ws-0.2.md
- **read_only_paths:** OVERVIEW.md, .metis/adr/, .metis/plans/phase-0-plan.md, Cargo.toml, crates/ship/tests/dependency_rule.rs

## Definition of Done

- [ ] Seven id types round-trip through `Display`/`FromStr` and serde; parsing rejects a wrong prefix, wrong length, uppercase, invalid characters and overflow.
- [ ] Slug validation accepts and rejects per the rule above.
- [ ] `compose_project_name(&EnvironmentId)` is deterministic, matches `^[a-z0-9][a-z0-9_-]*$`, and its only input is an `EnvironmentId`.
- [ ] Desired-state and operation-phase transition functions return an error for every illegal pair; a test enumerates all pairs against an explicit legal table.
- [ ] Service status derivation yields `healthy` only with a passing check and `unknown` whenever Docker is unreachable.
- [ ] `ApiError` serialises to exactly `code`, `message`, `details`, `operation_id`; every `ErrorCode` string is stable and unique.
- [ ] `HostInfo` and `ProjectList` round-trip through JSON; `Timestamp` formats/parses RFC 3339 for known values.
- [ ] `SecretValue` is not revealed by `{}`, `{:?}`, `{:#?}`, or inside a derived `Debug` of a containing struct.
- [ ] `ship-core` depends on no workspace crate; source contains no `std::fs`, `std::net`, `std::process`, `std::env`, `SystemTime` or `Instant` (test greps `src/`).
- [ ] `metis verify --post` green.

## Test plan

- Unit tests in each `crates/ship-core/src/*.rs` module:
  - `id`: round-trip for all seven types, known encoding vector, wrong prefix, other entity's prefix, wrong length, uppercase, `i/l/o/u` and punctuation, overflow first char > `7`, serde round-trip and serde rejection.
  - `slug`: accept/reject table.
  - `compose`: determinism, charset regex, distinct ids give distinct names.
  - `state`: all-pairs tables for `DesiredState` and `OperationPhase`; `OperationStatus` mapping; service-status derivation table over (reachable, container state, health).
  - `error`: JSON keys exact, `details` default `{}`, `operation_id` null/present, code strings unique and snake_case.
  - `api`: `HostInfo`/`ProjectList` JSON round-trip; `Timestamp` known vectors (epoch, leap day, pre-1970 rejected or handled), parse rejects malformed.
  - `secret`: Display/Debug/alternate Debug/nested Debug do not contain the value; `expose()` returns it.
- `crates/ship-core/tests/purity.rs`: scans `crates/ship-core/src/**/*.rs` for forbidden I/O paths.
- Gate: `metis verify --post` (`make verify`).

## Out-of-scope touches

None.

## Report

Filled at implementation.
