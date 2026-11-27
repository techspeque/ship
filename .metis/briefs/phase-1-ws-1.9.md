# phase-1-ws-1.9 — `ship push`

- **Type:** feat | **Risk:** high | **Priority:** p2
- **Coder:** claude-code/opus-5-5 | **Date:** 2026-11-27
- **Plan:** .metis/plans/phase-1-plan.md §1.9

## Goal

`ship push [PATH]` sends a working tree to the target's daemon as one
`push` operation (upload, then the deployment of 1.7/1.8), follows its
events with `--wait` (resuming after a dropped connection), prints the
result and the real endpoint, and exits with the codes of
`docs/contracts/cli-json.md`; `-o json` matches the push fixtures; plus
`ship settings get|set compose.confirm (ask | accept) [--project P]`.

## Architectural context

- ADR-0004 (JSON contract, exit codes 90–99), ADR-0005 (snapshot, dry
  run), ADR-0008 rule 2 (confirmation; an agent cannot confirm by itself),
  ADR-0011 (one persisted operation before any side effect; SSE resume by
  sequence), ADR-0006 (`services[].status` is what Docker reports now),
  OVERVIEW §3.3 (no address printed that does not exist).
- The push fixtures show one `operation_id` across `uploading`,
  `validating`, `building`, … and a result naming the release, the
  deployment and each service's status and endpoints. No endpoint serves
  that today, and the upload and deploy endpoints are two operations, so
  the daemon gains a `push` operation and one read. `deploy::run` and
  `upload::receive` are already public "so an operation that carries an
  upload before it (`ship push`) runs the same steps".
- New in `ship-core::api`:
  - `pushes_path(project)` → `POST /api/v1/projects/{project}/pushes`:
    body the snapshot (as uploads, same headers), plus the query
    `file=<percent-encoded>&port=<N|HOST:CONTAINER>&confirm=true`; answers
    `202` with `PushView { operation: OperationView, project_id,
    environment_id, revision_id }` once the snapshot is stored, or the
    upload's error with `operation_id`. `PUSH_KIND = "push"`,
    `PROJECT_KIND = "project"`.
  - `operation_deployment_path(id)` → `GET
    /api/v1/operations/{id}/deployment`: `OperationDeploymentView {
    deployment_id, release_id, result, services: [ServiceOutcomeView {
    name, status, endpoints: [EndpointView { provider, address }] }] }`;
    `404 not_found` when the operation recorded no deployment.
  - `percent_encode` / `percent_decode` for the query value.
- New in `ship-store`: `deployment_for_operation(op) ->
  Option<Deployment>`.
- New in `ship-daemon`:
  - `push.rs`: the pushes handler. The project, if new, is created by its
    own `project` operation (key `<key>/project`), so nothing is written
    outside an operation; then the `push` operation on the project's
    `development` environment (holding its lock) runs
    `upload::receive` and `deploy::run`. A replayed key answers the
    original operation without reading the body.
  - `Context::continue_in(phase, message)`: enters `phase`, or, when the
    operation is already in it, logs `message`; `deploy::run` uses it for
    `validating`, so a deployment after an upload keeps phases strictly
    increasing.
  - The deployment read: the operation's deployment; for a succeeded one,
    each service of the environment with the status Docker reports now
    for that release's containers (`unknown` when Docker is unreachable)
    and the routes recorded for it, each listed only if a TCP connection
    to it answers within a short timeout (`0.0.0.0:N` is tried at
    `127.0.0.1:N`). A failed deployment lists no services.
- New in `ship-net::client`: `get_stream` — a `GET` whose answer body is
  delivered chunk by chunk (SSE), with the ssh child alive until the
  stream is dropped.
- New in `ship` (the binary): `push.rs`, the command; `settings.rs` gains
  `compose.confirm` with `--project` and `--confirm`; a `-o json` writer
  for the push lines; terminal detection by `isatty` on stdin and stderr.
- Consumed unchanged: `ship-source::{scan, pack, DryRunReport}` (now a
  normal dependency of `ship`), `deploy::run`, `upload::receive`, the
  queue, the SSE endpoint, the settings endpoints.

### Behaviour of `ship push`

- `PATH` defaults to `.`; `--project` defaults to the directory's name,
  which must be a valid slug (else exit 2 naming `--project`).
- `--to TARGET` uses that target instead of the current one.
- `--wait` is the default; `--detach` prints the operation id and returns
  0 once the snapshot is stored; both together are a usage error.
- `--dry-run` scans and prints the report (text, or the report as JSON
  with `-o json`), uploads nothing, exits 0.
- The snapshot is packed to a temporary file (its SHA-256 is a header,
  sent before the body), then streamed.
- With `--wait`, events are printed as they arrive (`-o json`: one
  `{"type":"event",…}` line each, then one `{"type":"result",…}` line);
  after a dropped stream the client reconnects with `Last-Event-ID` for up
  to `--reconnect SECS` (default 60), printing nothing twice and
  skipping nothing.
- Confirmation: when the operation fails `confirmation_required` and
  stdin and stderr are terminals, the items are shown and the operator is
  asked; a yes posts the same revision to the deployments endpoint with
  `confirm: true` and follows that operation. With no terminal and no
  `--confirm`, the items are printed and the exit is 3. `--confirm` sends
  `confirm=true` with the push.
- Text result: `Deployed <slug> (release …)`, then per service `name:
  status`, then `Endpoint: <address>` for each endpoint, or `Endpoint: not
  configured` when there is none.
- Exit codes: 0 succeeded; 1 the operation failed with a code describing
  what was pushed (`build_failed`, `build_timeout`, `deploy_failed`,
  `port_conflict`, `health_check_failed`, `no_build_definition`,
  `compose_rejected`, `invalid_request`, `digest_mismatch`,
  `payload_too_large`, `unsafe_snapshot`), or the tree could not be
  packed; 2 usage; 3 confirmation required; 90 no daemon answered (no
  socket, ssh unreachable, connect timeout, `ship` missing on the host);
  91 `ClientError::Incompatible`; 92 failure code `docker_unavailable`; 93
  the operation ended `interrupted`; 94 the stream was lost and not
  regained within `--reconnect`; 99 anything else (`internal`,
  `queue_full`, `insufficient_storage`, a code this build does not know,
  an answer it cannot read).

### `ship settings … compose.confirm`

- `get compose.confirm [--project P]`: the host value, or the project's
  value in effect.
- `set compose.confirm accept [--project P]` prints what it waives (every
  Compose file of the host or project deploys without asking, the items
  are recorded as accepted by policy) and asks on a terminal; with no
  terminal it needs `--confirm`, else exits 3. `ask` needs no
  confirmation. Values other than `ask`/`accept` are exit 2.

## Declared file scope

- **owned_paths:**
  - crates/ship/
  - crates/ship-core/src/api.rs
  - crates/ship-store/src/history.rs
  - crates/ship-store/tests/
  - crates/ship-daemon/src/push.rs
  - crates/ship-daemon/src/http.rs
  - crates/ship-daemon/src/lib.rs
  - crates/ship-daemon/src/queue.rs
  - crates/ship-daemon/src/deploy.rs
  - crates/ship-daemon/src/upload.rs
  - crates/ship-daemon/tests/push.rs
  - crates/ship-daemon/Cargo.toml
  - crates/ship-net/src/client.rs
  - crates/ship-net/Cargo.toml
  - Cargo.lock
  - .metis/briefs/phase-1-ws-1.9.md
- **read_only_paths:** .metis/adr/, .metis/plans/phase-1-plan.md,
  OVERVIEW.md, docs/contracts/cli-json.md, crates/ship-source/,
  crates/ship-docker/, the rest of crates/ship-daemon/ and
  crates/ship-store/, .github/workflows/ci.yml

## Definition of Done

- [ ] `ship push .` deploys a Dockerfile project and a Compose project
      over the local socket and over ssh (fake `ssh` running `ship
      dial-stdio` locally; the real sshd in `ssh_real.rs`, CI `ssh` job),
      against the fake `docker`.
- [ ] Every exit code in the contract (0, 1, 2, 3, 90, 91, 92, 93, 94, 99)
      is produced by a test that creates its condition.
- [ ] `--wait -o json` output, with ids, timestamps and messages masked,
      has the fixtures' line types, field names and value types, ends with
      exactly one result line, and sequences increase by one.
- [ ] `--dry-run` sends no request to the daemon and lists `.env` and a
      private key as excluded.
- [ ] A connection dropped during the build (a proxy that cuts the stream
      once) is regained, and the printed sequences are 1..N with no gap
      and no repeat.
- [ ] With no route the output says `Endpoint: not configured` and the
      JSON lists `endpoints: []`; a route whose address does not answer is
      not printed.
- [ ] `--detach` prints the operation id and exits 0 before the build ends.
- [ ] Compose confirmation: no terminal and no `--confirm` exits 3 and
      prints the items; `--confirm` deploys.
- [ ] `ship settings set compose.confirm accept --project P` without a
      terminal and without `--confirm` exits 3 and changes nothing; with
      `--confirm` it changes the project's value and `get --project P`
      shows it.
- [ ] The daemon's `push` operation has strictly increasing phases, holds
      the environment lock, and a replayed key starts nothing new.
- [ ] `metis verify --post` green.

## Test plan

- Unit (`ship`): exit code mapping per failure code and per client error;
  slug from directory name; the JSON line writer against the fixtures'
  shapes; text rendering of results with and without endpoints; flag
  conflicts (`--wait`/`--detach`); SSE frame parsing across chunk
  boundaries; resume bookkeeping (no repeat after reconnect).
- Unit (`ship-core::api`): path builders, `PushView` and
  `OperationDeploymentView` shapes, percent encoding round trip.
- `ship-store/tests`: `deployment_for_operation`.
- `ship-daemon/tests/push.rs` (fake `docker`): push of a new project
  creates it through a `project` operation and deploys in one `push`
  operation with increasing phases; a replay starts nothing; the
  deployment read with a live and a dead endpoint; a failed build lists no
  services; `404` for an operation without a deployment.
- `ship/tests/cli_push.rs` (built binary, in-process daemon, fake
  `docker`, fake `ssh`, a cutting proxy, fake daemons for 91 and 99): each
  exit code, JSON shape against the fixtures, dry run, detach, reconnect,
  confirmation, settings `compose.confirm`.
- `ship/tests/ssh_real.rs`: `ship push` over the real sshd (ignored; CI).

## Out-of-scope touches

None.

## Report

Filled at implementation.
