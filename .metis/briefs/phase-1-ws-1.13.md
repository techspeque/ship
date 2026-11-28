# phase-1-ws-1.13 — UI for projects, services and logs

- **Type:** feat | **Risk:** medium | **Priority:** p2
- **Coder:** claude-code/opus-5-5 | **Date:** 2026-11-28
- **Plan:** .metis/plans/phase-1-plan.md §1.13

## Goal

The embedded UI shows the project list and a project page (services with
desired state, last deployment and observed status as three separate
things), deployment history with failures and their phase, each service's
logs with follow, an operation's progress and log live over SSE resuming
after a dropped connection, config values and secret names (never values),
and offers start, stop, restart, redeploy and remove, the disruptive ones
behind a confirmation naming what they affect; every new wire shape has a
contract fixture from the Rust type.

## Architectural context

- ADR-0006 (desired / last deployment / observed kept apart; `healthy`
  only from a health check), ADR-0011 (mutations are operations with an
  idempotency key; events over SSE resume from a sequence), ADR-0009
  (secret values never answered), ADR-0003 rule 6 / OVERVIEW §5 (mutations
  same-origin; `origin_guard` unchanged), ADR-0012 (no third-party origin;
  CSP `connect-src 'self'`).
- The daemon API is consumed unchanged (all routes exist after 1.11/1.12):
  - `GET /api/v1/projects` → `ProjectList`
  - `GET /api/v1/projects/{p}/status` → `StatusView`
  - `GET /api/v1/projects/{p}/deployments` → `DeploymentList`
  - `GET /api/v1/projects/{p}/logs?service=S&follow=true&tail=N` → NDJSON
    of `LogLineView`, blank keep-alive lines, cut short on a read failure
  - `GET /api/v1/projects/{p}/vars[?env=E[&service=S]]` → `VarList`;
    `…/secrets` → `SecretList` (names and `updated_at` only)
  - `POST /api/v1/projects/{p}/{start|stop|restart|redeploy|remove}
    [?service=S]`, no body, `Idempotency-Key` → `202 OperationView`
  - `GET /api/v1/operations/{id}` → `OperationView`;
    `GET /api/v1/operations/{id}/events` SSE, `id:` = sequence, `data:` =
    `OperationEventView`, resumes after `Last-Event-ID`, ends after the
    `finished` event.
- `web/src/lib/api.ts`: hand-written wire types with exact-shape guards
  for every shape above; `getJson` unchanged; `postAction` (lifecycle POST
  with a fresh `crypto.randomUUID()` idempotency key, `credentials:
  'same-origin'`, no body).
- `web/src/lib/sse.ts`: a `fetch`-based SSE reader (not `EventSource`,
  which reconnects forever after the stream ends and cannot be tested in
  node): parses frames, follows an operation's events, reconnects after a
  dropped connection with `Last-Event-ID` = last sequence applied, drops
  any event at or below it, stops at `finished`.
- `web/src/lib/ndjson.ts`: streamed NDJSON lines for logs (blank lines
  skipped, abortable); an answer that ends with an error is shown as
  "cut short", never as complete.
- `web/src/lib/actions.ts`: which actions apply to a project or a
  service (`redeploy`/`remove` project only), which need confirmation
  (`stop`, `restart`, `redeploy`, `remove`: each interrupts what runs;
  `start` does not), the confirmation text naming project, environment
  and services affected (and for `remove`: containers and networks go,
  named volumes and history stay); `submitAction` refuses a confirmable
  action without a confirmation, before any request.
- `web/src/lib/view.ts` (and siblings): pure API → page mappings; state
  strings are shown exactly as the API sends them.
- Routing by URL hash (`#/`, `#/projects/{slug}`,
  `#/projects/{slug}/logs[/{service}]`, `#/operations/{id}`): the daemon's
  SPA fallback is unchanged and nothing is sent to the server for a route.
- Confirmation is a native `<dialog>` opened with `showModal()` (focus
  trapped, Escape cancels); every control is a `<button>` or `<a href>`.
- Rust half of the drift guard: `crates/ship-daemon/tests/web_contract.rs`
  gains struct-literal samples of `StatusView`, `DeploymentList`,
  `LogLineView`, `VarList`, `SecretList`, `OperationView`,
  `OperationEventView`, and the lists of `OperationPhase`,
  `OperationStatus`, `DesiredState`, `DeploymentResult`, `EventKind`.

## Declared file scope

- **owned_paths:**
  - web/src/
  - crates/ship-daemon/tests/web_contract.rs
  - .metis/briefs/phase-1-ws-1.13.md
- **read_only_paths:** crates/ (other than the test above), docs/,
  .metis/adr/, .metis/plans/phase-1-plan.md, OVERVIEW.md, web/package.json,
  web/package-lock.json, web/vite.config.ts

## Definition of Done

- [ ] Project list links to each project's page; the page shows each
      service's desired state, the last deployment (action, result, times)
      and the observed status as separate labelled values, equal to the
      `StatusView` the API returned (test over the fixtures).
- [ ] Deployment history lists every deployment with result, and for a
      failed or interrupted one its failure code, phase and message; each
      links to its operation view.
- [ ] Logs per service (or all), with a follow toggle; stdout and stderr
      told apart; a cut-short answer says so.
- [ ] Operation view: progress (phase) and log lines live over SSE; after a
      dropped connection it resumes with `Last-Event-ID` and no line is
      lost or repeated (test with a fake stream dropped mid-way).
- [ ] Start, stop, restart, redeploy, remove reachable from the project
      page (start/stop/restart per service too); stop, restart, redeploy,
      remove cannot be sent without the confirmation (test); the
      confirmation names what is affected; after submit the operation view
      follows the returned operation.
- [ ] Config values per scope (project, environment, each service) and
      secret names with `updated_at`; the secret guards reject any shape
      carrying a value; the UI sends no request with a secret value (it has
      no secret-writing request at all; test over every request it builds).
- [ ] Every action reachable by keyboard: native buttons and links, a
      modal dialog with Escape, visible focus ring.
- [ ] Contract fixtures for every new shape, byte-equal to the Rust
      serialisation; the TS guards accept each and reject extra, missing
      and mistyped keys.
- [ ] `metis verify --post` green (lint, svelte-check, vitest, build,
      fmt, clippy, cargo test).

## Test plan

- `web_contract.rs`: new samples serialised and compared with
  `web/src/lib/contract/*.json` (regenerated with
  `SHIP_UPDATE_CONTRACT=1`); enum lists in declaration order.
- `contract.test.ts`: every new fixture passes its guard; TS enum lists
  equal the Rust ones; extra / missing / wrong-typed keys rejected;
  a secret entry with `value` rejected.
- `view.test.ts` (extended) / `project.test.ts`: status, deployments and
  config views over the fixtures show exactly the API's strings in the
  three separate columns; failures with phase.
- `sse.test.ts`: frame parser (multi-line data, comments, CRLF, split
  chunks); follower against a fake `fetch` that drops the connection
  mid-stream and re-serves from the `Last-Event-ID` it receives, including
  overlap: every sequence applied exactly once, in order, stops at
  `finished`; a gap is reported.
- `ndjson.test.ts`: split chunks, blank keep-alive lines, a stream ending
  in an error reported as cut short.
- `actions.test.ts`: availability per target; confirmation required for
  stop/restart/redeploy/remove and not start; `submitAction` without
  confirmation sends nothing; the request: method POST, path, `service`
  query, idempotency key, no body; confirmation text names project,
  environment and services, and volumes kept for remove.
- `secrets.test.ts` (or within the above): every request the UI's API
  module builds, against a recording `fetch`, carries no body except none,
  and never a known test secret.

## Out-of-scope touches

None.

## Report

- Changed: `web/src/App.svelte` (hash router, focus to the page heading on
  route change); `web/src/lib/`: `api.ts` (wire types and exact guards
  for status, deployments, log line, vars, secrets, operation, event;
  enum lists; path builders; `postAction`), `sse.ts` (parser,
  `followOperation`), `ndjson.ts` (`readNdjson`, `streamLogs`),
  `actions.ts`, `route.ts`, `project.ts`, `load.ts` (`load`,
  `loadConfig`), `view.ts` (environments on project rows); components
  `Home`, `ProjectPage`, `OperationPage`, `LogsPage`, `Config`,
  `ConfirmDialog`, `Badge`, `RefreshButton`, `Projects` (links,
  environments); 16 new fixtures in `web/src/lib/contract/`;
  `crates/ship-daemon/tests/web_contract.rs` (samples for every new shape
  and six enum lists).
- Tests added: `project.test.ts` (8), `sse.test.ts` (9), `ndjson.test.ts`
  (7), `actions.test.ts` (12), `secrets.test.ts` (2), `route.test.ts` (3),
  `contract.test.ts` (+8), `view.test.ts` (+1). Web: 77 tests pass.
- `metis verify --post --slice phase-1-ws-1.13`: ALL GREEN
  (`.metis/runs/phase-1-ws-1.13/verify-post.log`; `fixtures_match_the_rust_types ... ok`).
- Decisions: confirmation for every action but `start` (each of stop,
  restart, redeploy, remove interrupts what runs); `fetch`-based SSE, not
  `EventSource` (which reconnects forever after the terminal event); hash
  routes, so the daemon's SPA fallback is untouched; config shown for
  project, environment and each service scope; the UI writes no config or
  secret (plan asks only to show them); logs request `tail=200`, keep the
  last 5 000 lines on the page.
- Process note: `cargo` and `rustfmt` were not runnable from this session
  (permission), so the 16 fixtures were written by a one-off node script
  (since deleted) mirroring the Rust samples; `fixtures_match_the_rust_types`
  in verify confirms them byte for byte, and `cargo fmt --check` passed.
