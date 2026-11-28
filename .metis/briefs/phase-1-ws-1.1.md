# phase-1-ws-1.1 — Status model and error decoding fixes

- **Type:** feat | **Risk:** medium | **Priority:** p2
- **Coder:** claude-code/opus-5-5 | **Date:** 2026-11-27
- **Plan:** .metis/plans/phase-1-plan.md §1.1

## Goal

Fix the Phase 0 status model and client error decoding so Phase 1 can build
on them: service status derived from the observation and the environment's
desired state (with `paused` and the OOM flag), a daemon error with an
unknown code decoded without losing its message, never read as
"compatible", and the error codes Phase 1 needs.

## Architectural context

- ADR-0006 rule 2 (amended 2026-11-27): eight service statuses `healthy`,
  `running_unverified`, `starting`, `unhealthy`, `failed`, `stopped`,
  `paused`, `unknown`; an exited container under desired `stopped` is
  `stopped` whatever its exit code. Rule 4: Docker unreachable is `unknown`.
- `ship_core::state`: `ServiceStatus`, `ContainerState`, `HealthCheck`,
  `Observation`, `DesiredState`. `ServiceStatus::from_observation` gains the
  desired state as an argument. `Observation::Reachable` gains
  `oom_killed: bool`. New `FailureDetail` (reason, exit code, OOM flag) is
  returned exactly when the status is `failed`.
- `ship_core::error`: `ErrorCode` (string_enum; codes only appended, never
  renamed or reordered) and `ApiError` (what the daemon sends). New
  `ReceivedCode` (`Known(ErrorCode)` | `Unknown(String)`) and
  `ReceivedError` (what a client decodes: code, message, details,
  operation_id) for tolerant decoding. Wire shape unchanged.
- `crates/ship/src/client.rs`: `get_json` and `check_version` decode error
  bodies as `ReceivedError`. `ClientError::Api` carries a `ReceivedError`.
  Any 426 answer, or any body whose code is `unsupported_api_version`, is
  incompatible whatever else it carries.
- Web drift guard: `crates/ship-daemon/tests/web_contract.rs` writes
  `web/src/lib/contract/*.json`; `web/src/lib/contract.test.ts` runs the
  guards in `web/src/lib/api.ts` over them. A `service-statuses.json`
  fixture and `SERVICE_STATUSES` / `isServiceStatus` are added the same way
  as the Docker statuses.

New error codes, appended, each with its HTTP status:

| code | HTTP | used by |
|---|---|---|
| `payload_too_large` | 413 | upload over the size or file-count limit (1.4, 1.5) |
| `unsafe_snapshot` | 422 | unpack refuses an entry (1.5) |
| `digest_mismatch` | 422 | uploaded snapshot digest differs (1.5) |
| `no_build_definition` | 422 | neither Dockerfile nor Compose file found (1.7, 1.8) |
| `compose_rejected` | 422 | trust profile rejects the file (1.8) |
| `confirmation_required` | 428 | trust profile needs confirmation (1.8, 1.9) |
| `idempotency_conflict` | 422 | idempotency key reused with a different request (1.3) |
| `environment_busy` | 409 | a mutation already runs on the environment (1.3) |
| `build_failed` | 422 | image build failed (1.7; in `push-wait-failed.jsonl`) |
| `build_timeout` | 504 | build exceeded its timeout (1.7) |
| `port_conflict` | 409 | host port already in use (1.7) |
| `deploy_failed` | 422 | `compose up` failed (1.7, 1.8) |
| `health_check_failed` | 422 | verification failed (1.7) |
| `insufficient_storage` | 507 | disk watermark reached (1.5, 1.7) |
| `interrupted` | 503 | daemon stopped during the operation (1.14) |

## Declared file scope

- **owned_paths:**
  - crates/ship-core/src/state.rs
  - crates/ship-core/src/error.rs
  - crates/ship-core/src/lib.rs
  - crates/ship/src/client.rs
  - crates/ship/src/commands.rs
  - crates/ship-daemon/tests/web_contract.rs
  - web/src/lib/api.ts
  - web/src/lib/contract.test.ts
  - web/src/lib/contract/
  - .metis/briefs/phase-1-ws-1.1.md
- **read_only_paths:** .metis/adr/, .metis/plans/phase-1-plan.md,
  docs/contracts/cli-json.md, crates/ship-daemon/src/, crates/ship-net/src/,
  crates/ship/tests/

## Definition of Done

- [ ] A daemon error with an unknown code is printed with the daemon's
      message and the raw code; details and operation id are kept.
- [ ] A 426 answer, or an error body whose code is `unsupported_api_version`,
      is incompatible even with a matching version header; an unknown code
      on another status is an API error, never "compatible" silently.
- [ ] Exit codes 137 and 143 under desired `stopped` give `stopped`; under
      `running` they give `failed`. `removed` is treated like `stopped`.
- [ ] A paused container gives `paused`, never `stopped`, under every
      desired state and health.
- [ ] Docker unreachable gives `unknown` under every desired state.
- [ ] `Observation::Reachable` carries `oom_killed`; `FailureDetail` is
      present exactly when the status is `failed` and names the OOM kill.
- [ ] The new error codes exist with the HTTP statuses above; the existing
      eleven keep their strings and positions.
- [ ] `service-statuses.json` is generated from the Rust type; the TS list
      equals it in order; `isServiceStatus` accepts every status, including
      `paused`, and rejects anything else.
- [ ] `metis verify --post` green.

## Test plan

- `ship-core` `state` tests: full table over every container state × health
  × desired state × OOM flag, written independently of the code; exit codes
  0, 1, 137, 143, -1; `unknown` for Docker unreachable under every desired
  state; `FailureDetail` present iff `failed`, its text; string forms
  include `paused` in the ADR order.
- `ship-core` `error` tests: stable code list (old eleven first, unchanged);
  each new code's HTTP status; `ReceivedError` decodes a known code, an
  unknown code (raw string kept with message, details, operation id), and
  missing optional fields; round trip from `ApiError`.
- `ship` `client` unit tests: unknown code on a 4xx gives `ClientError::Api`
  whose text has the daemon's message and raw code; 426 with an unknown code
  or a non-error body is `Incompatible`; `unsupported_api_version` on a
  non-426 status is `Incompatible`.
- `web_contract.rs`: `service-statuses` fixture matches the Rust list.
- `contract.test.ts`: TS `SERVICE_STATUSES` equals the fixture in order,
  includes `paused`; `isServiceStatus` rejects an unknown string and a
  non-string.

## Out-of-scope touches

None.

## Report

- `ship-core/src/state.rs`: `ServiceStatus::Paused` (ADR order, before
  `unknown`); `from_observation(observation, desired)`; exited under
  `stopped`/`removed` → `stopped`; `Observation::Reachable.oom_killed`;
  `FailureReason`, `FailureDetail` (Some iff `failed`, Display names OOM).
- `ship-core/src/error.rs`: fifteen codes appended with HTTP statuses per the
  table; `ReceivedCode`, `ReceivedError` (same wire shape, unknown code kept
  raw). `lib.rs` re-exports.
- `ship/src/client.rs`: error bodies decoded as `ReceivedError`
  (`decode_error`); `check_version` refuses any 426 and any
  `unsupported_api_version` body on any status.
- Web: `service-statuses.json` (written by hand to the exact
  `to_string_pretty` form; the env-prefixed regeneration command was not
  permitted in this session, and `fixtures_match_the_rust_types` passing
  confirms the bytes), `SERVICE_STATUSES` / `isServiceStatus` in `api.ts`,
  three tests in `contract.test.ts`.
- `crates/ship/src/commands.rs` was declared but needed no change.
- `metis verify --post --slice phase-1-ws-1.1`: ALL GREEN (all new Rust and
  vitest tests listed in the test plan ran and passed).
