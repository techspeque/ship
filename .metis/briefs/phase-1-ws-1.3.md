# phase-1-ws-1.3 — Operation queue, events and logs

- **Type:** feat | **Risk:** high | **Priority:** p2
- **Coder:** claude-code/opus-5-5 | **Date:** 2026-11-27
- **Plan:** .metis/plans/phase-1-plan.md §1.3

## Goal

Add the daemon's durable operation queue (ADR-0011): every mutation is a
persisted operation before any side effect, admitted only with an
idempotency key, serialised per environment with one host-wide build slot,
its events streamed over resumable SSE and its output written to bounded,
redacted on-disk logs; operations left unfinished are marked `interrupted`
on daemon start and never resumed.

## Architectural context

- ADR-0011 rules 1–10; OVERVIEW §2.2 (Operation, OperationEvent), §3.3
  (every mutation persisted first, no concurrent mutations per
  environment, no secret in logs or events), §6 (202 + operation id, SSE
  at `/api/v1/operations/{id}/events` resuming from a sequence), §8
  (limits: one build at a time, build timeout, disk watermark).
- Store API from phase-1-ws-1.2, consumed unchanged: `create_operation`
  (idempotent on the key; `Conflict` when replayed with another kind or
  environment), `get_operation`, `advance_operation`, `fail_operation`,
  `set_log_cursor`, `unfinished_operations`, `append_event`,
  `events_after`. The schema's triggers already enforce phase order and
  terminal finality.
- `ship-core`: `OperationPhase`, `OperationStatus`, `ErrorCode`,
  `ReceivedCode`, `ApiError` (has `operation_id`).
- `ship_daemon::Redactor` (log.rs) is the redacting logger; operation log
  lines and event messages go through it before any byte is persisted.
- New in `ship-core`:
  - `ErrorCode::QueueFull` (`queue_full`, HTTP 429), appended; nothing
    renumbered.
  - `api.rs`: `OPERATIONS_PATH`, `IDEMPOTENCY_KEY_HEADER`
    (`idempotency-key`), `LAST_EVENT_ID_HEADER`, `OperationView` (id,
    kind, environment_id, phase, status, failure `{code, phase, message}`
    or null, log_bytes, created_at, updated_at), `OperationEventView`
    (operation_id, sequence, phase, event, message, ts — the field names
    of the `push-wait-*.jsonl` event lines), `EventKind`
    (`phase_started` | `log` | `waiting` | `finished`).
- New in `ship-daemon`:
  - `queue` module: `Queue` (`submit`, `shutdown`), `Submission`,
    `Submitted`, `Limits` (max_queue, build_slots, build_timeout,
    disk_watermark_bytes, max_log_bytes; conservative defaults), `Context`
    (`enter(phase, message)`, `log(line)`, `build(future)`), `JobFailure`,
    `recover` (start-up interruption).
  - One worker task per environment draining an mpsc channel: mutations on
    one environment run one at a time in admission order; on two
    environments, concurrently. A build waits on a host-wide semaphore of
    `build_slots` permits and runs under `build_timeout`
    (`build_timeout` failure).
  - Admission: the operation row is created first; a replayed key returns
    the original operation and starts nothing. A new operation over
    `max_queue` in flight, or with the state directory's free disk under
    the watermark (or unreadable), is failed at once in phase `queued`
    with `queue_full` / `insufficient_storage` before its job runs.
  - Each job's terminal event is appended before the terminal phase is
    stored, so a reader that sees a terminal operation finds every event.
    A panicking job fails as `internal`.
  - Logs: `<state_dir>/operations/<op id>.log`, directory 0700, file
    0600, JSON lines `{ts, phase, line}`, each line redacted; capped at
    `max_log_bytes` plus one truncation marker line; `log_cursor` is the
    file's byte length. Each kept line is also a `log` event.
  - HTTP: `GET /api/v1/operations/{id}` (operation view);
    `GET /api/v1/operations/{id}/events` (SSE, `id:` = sequence, data =
    `OperationEventView` JSON; resumes after `Last-Event-ID` or
    `?after=N`; ends after the terminal event). `http::accept` is the one
    helper later mutation routes call: requires `Idempotency-Key`
    (1–128 visible ASCII; else `400 invalid_request`), returns `202` with
    the operation view, a replay the same; a key reused for another
    request is `422 idempotency_conflict`; an operation refused at
    admission answers its failure code's status with `operation_id` set,
    on the first request and on every replay alike.
  - `start` marks every `queued`/`running` operation `interrupted` (code
    `interrupted`, a terminal event, the log file untouched) before any
    listener serves; `shutdown` aborts running jobs, leaving their rows
    for the next start to interrupt.
- No mutation route is added here: push (1.9) and lifecycle (1.12) bring
  them and call `http::accept`.

## Declared file scope

- **owned_paths:**
  - crates/ship-daemon/
  - crates/ship-core/src/api.rs
  - crates/ship-core/src/error.rs
  - Cargo.lock
  - .metis/briefs/phase-1-ws-1.3.md
- **read_only_paths:** .metis/adr/, .metis/plans/phase-1-plan.md,
  OVERVIEW.md, docs/contracts/cli-json.md, crates/ship/tests/fixtures/,
  crates/ship-store/, crates/ship-core/src/state.rs, crates/ship/

## Definition of Done

- [ ] Replaying a request with the same idempotency key returns the
      original operation, creates no second operation and runs the job
      once; the same key for a different request is
      `422 idempotency_conflict`; a missing or malformed key is `400`.
- [ ] Two mutations on one environment never run at the same time and run
      in admission order; mutations on two environments overlap; builds
      never exceed `build_slots` across the host; a build over
      `build_timeout` fails `build_timeout`.
- [ ] `GET /api/v1/operations/{id}` and its event stream answer while a
      mutation holds the environment.
- [ ] An SSE client reconnecting with `Last-Event-ID: n` (or `?after=n`)
      receives exactly the events after `n`, live and after the end, with
      none missing or repeated.
- [ ] An operation left `queued` or `running` (including by a daemon whose
      runtime was dropped mid-job) is `interrupted` after restart, with
      its events and log file intact, and its job is not re-run.
- [ ] A registered secret written through `Context::log` and in a failure
      message does not appear in the log file, the stored events, the SSE
      stream or the operation view.
- [ ] Over `max_queue`, or under the disk watermark, a request fails with
      `queue_full` / `insufficient_storage` before its job runs; the log
      stops at `max_log_bytes` with a truncation marker.
- [ ] `metis verify --post` green.

## Test plan

- `ship-core` unit: `queue_full` code string and status in the existing
  tables; `OperationView` / `OperationEventView` JSON shapes and round
  trips; `EventKind` strings.
- `ship-daemon` unit (`queue.rs`, `http.rs`): idempotency key validation;
  resume-point parsing (header, query, both absent, malformed); log
  truncation arithmetic.
- `crates/ship-daemon/tests/queue.rs` (real SQLite in temp dirs):
  replay runs once; conflict on reused key; per-environment serialisation
  and order with an overlap probe; two environments overlap; build slot
  of 1 never overlaps across environments; build timeout; queue_full and
  insufficient_storage fail before the job runs; panicking job fails
  `internal`; secret redacted in log file, events and failure; log cap
  and marker; log cursor equals the file length.
- `crates/ship-daemon/tests/operations_http.rs`: `http::accept` over a
  test route: 202, replay, 400, 422, refusal status with `operation_id`;
  GET operation (found, 404, bad id); SSE full stream, resume by header
  and by query after completion, and live resume mid-operation; reads
  answer while the environment is busy.
- `crates/ship-daemon/tests/recovery.rs`: rows left queued/running are
  interrupted on `start`; a daemon whose runtime is dropped mid-job
  leaves an operation that the next `start` interrupts, with events and
  log intact, and the job does not run again.

## Out-of-scope touches

None.

## Report

- `ship-core/src/error.rs`: `ErrorCode::QueueFull` (`queue_full`, 429),
  appended; code and status tables extended.
- `ship-core/src/api.rs`: `OPERATIONS_PATH`, `IDEMPOTENCY_KEY_HEADER`,
  `IDEMPOTENCY_KEY_MAX_LEN`, `LAST_EVENT_ID_HEADER`, `operation_path`,
  `operation_events_path`, `valid_idempotency_key`, `OperationView`,
  `OperationFailureView`, `EventKind`, `OperationEventView` (decodes the
  `push-wait` fixture event fields). Not re-exported from `lib.rs` (not
  owned); used as `ship_core::api::…`.
- `ship-daemon/src/queue.rs` (new): `Queue`, `Submission`, `Submitted`,
  `QueueError`, `Limits` (defaults: 32 queued, 1 build slot, 30 min build
  timeout, 1 GiB watermark, 16 MiB log), `Context` (`enter`, `log`,
  `build`), `JobFailure`, `recover`, `LOG_DIR`, `INTERRUPTED_MESSAGE`.
  Per-environment worker over an mpsc channel; env-less operations get a
  task each; jobs run under `catch_unwind`.
- `ship-daemon/src/http.rs`: operation and SSE routes, `accept`,
  `submitted_response`, `operation_view`; SSE body built by hand
  (`Body::from_stream` + `futures_util::stream::unfold`), keep-alive
  comment every 15 quiet polls, stream ends on the terminal event or queue
  close.
- `ship-daemon/src/lib.rs`: `Config.limits`; `start` runs `recover`
  before any listener serves, then builds the queue; `Running::queue`;
  `shutdown` stops the queue first so event streams end before the drain.
- `ship-daemon/src/host.rs`: `disk_free_bytes` made `pub(crate)` for the
  watermark check.
- `ship-daemon/Cargo.toml`: `futures-util` 0.3 (already in the graph via
  axum); `Cargo.lock` adds it to ship-daemon's dependency list only.
- Tests: `tests/queue.rs` (12), `tests/operations_http.rs` (6),
  `tests/recovery.rs` (2), new unit tests in `queue.rs` and `http.rs`;
  `tests/common/mod.rs` gains a chunked/SSE reader and `limits` with no
  watermark.
- Decisions: a refusal at admission persists the operation and fails it
  in phase `queued` (so a replay is deterministic) and answers the
  failure code's status with `operation_id`, not 202. Free disk that
  cannot be read refuses. `Context::build` reports a `waiting` event when
  the slot is taken. On shutdown, running jobs are aborted and their rows
  left for `recover` at the next start (one code path for kill and stop).
  `recover` does not add a second `interrupted` event if one is already
  last.
- Unit test caught `fits` saturating at `u64::MAX`; fixed with
  `checked_add`.
- `metis verify --post --slice phase-1-ws-1.3`: ALL GREEN (web, fmt,
  clippy, all workspace tests).
