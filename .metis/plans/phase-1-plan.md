---
type: plan
phase: 1
title: Push
overview_ref: OVERVIEW.md §4 Phase 1
status: draft
created: 2026-11-27
---

# Phase 1 — Push

> Derived from: OVERVIEW.md §4 Phase 1
> Produces: slices phase-1-ws-1.1 … 1.14 and phase-1-gate in .metis/slices.yaml
> ADRs: 0005, 0006, 0007, 0008, 0009, 0011 (all in .metis/adr/)
> Contract: docs/contracts/cli-json.md

## Context

Phase 1 is the first usable release. After it, `ship push .` sends a working
tree to a Ship host, builds it, starts it and prints the real address; the
operator sees projects, services, state and logs in the CLI and the UI, sets
config and secrets, and starts, stops, restarts, redeploys and removes what
is running. Every mutation is a durable operation. There is one environment
per project, `development`.

Decisions made in this plan:

- **A stopped service is `stopped` whatever its exit code.** Status is
  derived from the observation and the environment's desired state, so exit
  137 or 143 after `ship stop` is not `failed`.
- **`paused` becomes an eighth service status.** ADR-0006 rule 2 is amended
  with this plan.
- **Routes are published ports only.** The route table is created with the
  `port` provider; tailnet and Caddy providers are Phase 2.
- **Confirmation without a terminal fails.** A Compose file that needs
  confirmation exits 3 unless the operator confirmed that same file before
  or passes `--confirm`. An agent session cannot confirm on its own.
- **The JSON shapes are already written.** The fixtures under
  `crates/ship/tests/fixtures/` came before the code; a slice matches them
  or changes them on purpose.

## Dependencies

- Requires: Phase 0 complete and v0.0.1 released
- External: a real Docker Engine in CI, as in Phase 0
- Local: the adoption gate's machines, addresses, the workloads moved onto them and the migration runbook are in `.local/`, gitignored and never committed (ADR-0012). Steps that need root, and stopping the orchestrator that ran those workloads before, are the Human's.

---

## Workstream 1.1: Status model and error decoding fixes

- **Risk:** medium
- **Coder:** claude-code/opus-5-5
- **Reviewer:** codex/gpt-6-astra
- **Stage:** push

Tasks:
- Decode an error body whose `code` the client does not know: keep `message`, `details` and `operation_id`, and show the raw code (`crates/ship/src/client.rs`, `ship-core/src/error.rs`)
- Apply the same tolerant decoding in the version check, so an unknown code is never read as "compatible"
- Derive service status from the observation and the environment's desired state: an exited container in an environment whose desired state is `stopped` is `stopped`, whatever its exit code (`ship-core/src/state.rs`)
- Add the `paused` service status for a paused container, in the core type, the UI guards and the web contract fixtures
- Extend `Observation` with the container's OOM-killed flag and report it in the failure detail
- Add the error codes Phase 1 needs, each with its HTTP status, without renumbering any existing code

Suggested packages:
- `crates/ship-core`
- `crates/ship`
- `web/src/lib`

Acceptance criteria:
- [ ] A daemon error with an unknown code is printed with the daemon's message
- [ ] Exit codes 137 and 143 under desired state `stopped` give `stopped`; the same codes under `running` give `failed`
- [ ] A paused container gives `paused`, never `stopped`
- [ ] Docker unreachable still gives `unknown` in every case
- [ ] The web contract test and the UI's type guards accept the new status

---

## Workstream 1.2: Store for Phase 1

- **Risk:** medium
- **Coder:** claude-code/opus-5-5
- **Reviewer:** codex/gpt-6-astra
- **Stage:** push
- **Blocked by:** 1.1

Tasks:
- Add a migration for route, config_value, secret (name, scope, backend reference and updated_at only) and observation (OVERVIEW §2.2)
- Add typed ids for route and the repositories for create, get and list on revision, release and deployment
- Add operation phase and status updates, failure recording, the log cursor, and a query for operations left `queued` or `running`
- Add the environment's desired-state update and append and list on audit_event
- Add get-or-create of a project with its `development` environment in one transaction
- Reject, in the schema, any update to a revision or release, as now, and any deployment without an operation

Suggested packages:
- `crates/ship-store`
- `crates/ship-core`

Acceptance criteria:
- [ ] An existing Phase 0 database migrates in place, with a backup, and reopens cleanly
- [ ] The secret table has no column that can hold a value
- [ ] An illegal operation phase or status transition is rejected by the store
- [ ] Two concurrent get-or-create calls for one slug yield one project and one environment
- [ ] Every repository function has a test against a real SQLite file

---

## Workstream 1.3: Operation queue, events and logs

- **Risk:** high
- **Coder:** claude-code/opus-5-5
- **Reviewer:** codex/gpt-6-astra
- **Stage:** push
- **Blocked by:** 1.2

Tasks:
- Add the in-process queue backed by the operation table: a mutation is a persisted operation before any side effect (ADR-0011)
- Accept mutating requests only with an idempotency key; return `202` and the operation id; a replayed key returns the original operation
- Hold one mutation lock per environment and one build slot across the host; reads are never blocked
- Serve `GET /api/v1/operations/{id}` and its events over SSE, resuming from a sequence number with nothing missed and nothing repeated
- Write operation output to bounded on-disk logs through the redacting logger
- On daemon start, mark operations left `queued` or `running` as `interrupted`; never resume one
- Enforce queue length, build timeout and the disk watermark before work starts, each with its typed failure

Suggested packages:
- `crates/ship-daemon`
- `crates/ship-core`

Acceptance criteria:
- [ ] Replaying a request with the same key creates no second operation and no second side effect
- [ ] Two mutations on one environment never run at the same time; mutations on two environments do
- [ ] An SSE client that reconnects with its last sequence receives exactly the events after it
- [ ] Killing the daemon mid-operation leaves the operation `interrupted` after restart, with its events and log intact
- [ ] A registered secret written to an operation log does not appear in the file or the event stream
- [ ] A request over the queue length or under the disk watermark fails before any side effect

---

## Workstream 1.4: Snapshot pack

- **Risk:** high
- **Coder:** claude-code/opus-5-5
- **Reviewer:** codex/gpt-6-astra
- **Stage:** push
- **Blocked by:** 1.1

Tasks:
- Walk a directory and select tracked, untracked and modified files not ignored by `.gitignore` or `.shipignore`, with no Git metadata required (ADR-0005 rule 1)
- Exclude by default `.git/`, `.env`, `.env.*` other than template files, private keys, credential stores and build directories; include one only when the operator names it (rule 2)
- Send `.dockerignore` as a file; do not apply it (rule 3)
- Do not follow symlinks; report each as skipped (rule 6)
- Stream tar and zstd with a SHA-256 of the stream, in bounded memory
- Refuse a home directory, a filesystem root and a tree over the size or file-count limit
- Implement the report behind `--dry-run`: file count, size, and every sensitive file excluded

Suggested packages:
- `crates/ship-source`

Acceptance criteria:
- [ ] An uncommitted and an untracked file are in the snapshot; an ignored file is not
- [ ] `.git/`, `.env`, `.env.local` and a private key are absent by default, and `.env.example` is present
- [ ] The same tree packs to the same hash twice
- [ ] Packing a tree of many large files stays under a fixed memory bound
- [ ] A symlink is never followed, including one pointing outside the tree
- [ ] `ship-source` depends only on `ship-core`

---

## Workstream 1.5: Upload and safe unpack

- **Risk:** high
- **Coder:** claude-code/opus-5-5
- **Reviewer:** codex/gpt-6-astra
- **Stage:** push
- **Blocked by:** 1.3, 1.4

Tasks:
- Add the upload endpoint: stream the snapshot into a new staging directory, validating each entry as it arrives (ADR-0005)
- Reject absolute paths, `..`, symlinks, hardlinks, devices, FIFOs, and paths that collide after Unicode or case normalisation (rule 5)
- Enforce size, file-count and expansion-ratio limits during the stream, and verify the stream hash at the end
- Promote a fully valid stream to an immutable revision identified by its hash; a failed stream leaves only a failed operation record (rule 7)
- Record the client's Git commit and dirty flag as information only
- Carry the upload over the local socket and over `ssh … ship dial-stdio`
- Add property and fuzz tests on the unpacker

Suggested packages:
- `crates/ship-source`
- `crates/ship-daemon`
- `crates/ship`

Acceptance criteria:
- [ ] No test archive, including each hostile case in rule 5, writes a byte outside its staging directory
- [ ] A decompression bomb is stopped by the expansion-ratio limit with bounded disk use
- [ ] A hash mismatch or a broken transfer leaves no revision and no staging directory
- [ ] Uploading the same tree twice yields the same revision
- [ ] The upload works over a real sshd in CI
- [ ] The fuzz target runs in CI for a fixed budget without a finding

---

## Workstream 1.6: Docker and Compose adapter

- **Risk:** high
- **Coder:** claude-code/opus-5-5
- **Reviewer:** codex/gpt-6-astra
- **Stage:** push
- **Blocked by:** 1.1

Tasks:
- Add build, `compose config`, `compose up`, `compose stop`, `compose start`, `compose restart`, `compose down` without volumes, `compose ps`, `inspect` and `logs` to `ship-docker`, each from structured argv
- Make `down -v`, `--volumes`, `volume rm` and a shell impossible to express through the adapter's API
- Select objects only by Ship's labels; add `dev.ship.project`, `dev.ship.environment`, `dev.ship.service` and `dev.ship.release` to everything created
- Parse inspect output into `Observation`, including health, restart count, exit code and the OOM flag
- Stream build and container output line by line to a sink, with stdout and stderr told apart
- Treat `compose config` output as sensitive: never logged (ADR-0008 rule 7)
- Test against the real Docker Engine in CI

Suggested packages:
- `crates/ship-docker`

Acceptance criteria:
- [ ] No adapter function can produce an argv that removes a volume, checked by a test over the argv builders
- [ ] An object without Ship's labels is never returned, stopped or removed
- [ ] Every created container, network and volume carries the four labels
- [ ] Docker unreachable is a distinct error from a command that ran and failed
- [ ] `ship-docker` depends only on `ship-core`

---

## Workstream 1.7: Dockerfile deployment

- **Risk:** high
- **Coder:** claude-code/opus-5-5
- **Reviewer:** codex/gpt-6-astra
- **Stage:** push
- **Blocked by:** 1.5, 1.6

Tasks:
- Detect a Dockerfile or a Compose file in a revision; `--file` overrides; with both present and no `--file`, Compose wins and the choice is stated
- For a Dockerfile, generate a single-service Compose project outside the snapshot, named from the environment id, publishing `--port` when given
- Run the deployment as an operation through its phases: build, prepare, deploy, verify (ADR-0011 rule 2)
- Verify: a passing health check gives `healthy`; with none, a container still running after a settle period gives `running_unverified`
- Record the release (image ids, config fingerprint) and the deployment with its previous release (ADR-0007)
- On any failure leave the previous release's containers and data as they were, and keep the evidence

Suggested packages:
- `crates/ship-daemon`
- `crates/ship-core`

Acceptance criteria:
- [ ] A Dockerfile project is built, started and reachable on its published port against real Docker
- [ ] A failing build, a port conflict and a failing health check each end as `failed` with a typed code and the phase, and the previous release still serves
- [ ] The generated Compose file is outside the snapshot and contains no secret value
- [ ] A named volume survives a redeploy
- [ ] With no published port, no endpoint is recorded

---

## Workstream 1.8: Compose import under the trust profile

- **Risk:** high
- **Coder:** claude-code/opus-5-5
- **Reviewer:** codex/gpt-6-astra
- **Stage:** push
- **Blocked by:** 1.7

Tasks:
- Evaluate the canonical `docker compose config` output against the profile in `ship-core`: allow, confirm or reject, each with the lines that caused it (ADR-0008 rules 1 to 3)
- Confirm covers bind mounts to host paths, published ports, added capabilities, GPU reservations and non-default restart policies; reject covers `privileged`, host namespaces, `devices:` and the Docker socket
- Add only labels, the project name, managed config and secrets, and resource limits; change no port, volume, network or privilege the file declares (rules 4 and 5)
- Return the confirmation items and the diff to the client before the first deployment; record a confirmation, with its file fingerprint, in the audit log
- Deploy several services as one operation; record one service row per Compose service
- Mount bind sources inside the snapshot read-only by default

Suggested packages:
- `crates/ship-core`
- `crates/ship-daemon`

Acceptance criteria:
- [ ] Every outcome in rules 1 to 3 has a test with a Compose file that triggers it
- [ ] A rejected file starts nothing and names the offending lines
- [ ] A file needing confirmation starts nothing until confirmed; the confirmation is in the audit log; the same file redeploys without asking again, and a changed file asks again
- [ ] A multi-service file with a named volume and a health check deploys against real Docker
- [ ] A host-path bind mount and a GPU reservation deploy after confirmation
- [ ] The interpolated Compose output appears in no log, event or API response

---

## Workstream 1.9: `ship push`

- **Risk:** high
- **Coder:** claude-code/opus-5-5
- **Reviewer:** codex/gpt-6-astra
- **Stage:** push
- **Blocked by:** 1.8

Tasks:
- Implement `ship push [PATH] [--project P] [--file F] [--port N] (--wait | --detach) [--dry-run] [--confirm] [--to TARGET]`
- Default the project slug to the directory name, validated; create the project and its `development` environment on first push
- With `--wait`, stream ordered progress, survive a reconnect by resuming the event stream, and exit with the codes in `docs/contracts/cli-json.md`
- With `-o json`, print one object per line ending with the result line, matching `push-wait-succeeded.jsonl` and `push-wait-failed.jsonl`
- Ask for confirmation on a terminal; with no terminal and no `--confirm`, print the items and exit 3
- Print the endpoint only when one exists; otherwise `Endpoint: not configured`
- With `--detach`, print the operation id and return

Suggested packages:
- `crates/ship`

Acceptance criteria:
- [ ] `ship push .` on a Dockerfile project and on a Compose project deploys over the local socket and over ssh
- [ ] Every exit code in the contract is produced by a test that creates its condition
- [ ] The JSON output matches the fixtures apart from ids, timestamps and messages
- [ ] `--dry-run` uploads nothing and lists the sensitive files excluded
- [ ] Dropping the connection during a build and reconnecting loses no progress line
- [ ] No address is printed that does not answer

---

## Workstream 1.10: Config values and write-only secrets

- **Risk:** high
- **Coder:** claude-code/opus-5-5
- **Reviewer:** codex/gpt-6-astra
- **Stage:** push
- **Blocked by:** 1.7

Tasks:
- Implement `ship vars set|unset|list` and `ship secrets set|unset|list`, with secrets read only from `--prompt` or `--stdin` (ADR-0009 rule 2)
- Store secret values in files under the state directory at mode 0600, owned by the `ship` user; return names and timestamps only (rule 3)
- Resolve precedence, lowest first: manifest defaults, project, environment, service (rule 1)
- At deploy, write values to a permission-restricted env file outside the snapshot and pass it to Compose; never to an image, a build argument or a saved Compose file (rule 5)
- Register every secret value with the redacting logger
- Mark services running with values older than the stored ones as `stale_config` (rule 6)
- State in the docs that Phase 1 secrets are protected by file permissions, not encryption (rules 3 and 9)

Suggested packages:
- `crates/ship`
- `crates/ship-daemon`
- `crates/ship-core`

Acceptance criteria:
- [ ] A secret value passed as an argument is refused before it is sent
- [ ] No API response, log, event, generated Compose file or image history contains a test secret, checked by a test that greps all of them
- [ ] A deployed service sees the config and secret values with the right precedence
- [ ] Changing a value changes nothing running, and `ship status` shows `stale_config` until a redeploy
- [ ] The secret files are mode 0600 and their directory 0700

---

## Workstream 1.11: Status, deployments and logs

- **Risk:** medium
- **Coder:** claude-code/opus-5-5
- **Reviewer:** codex/gpt-6-astra
- **Stage:** push
- **Blocked by:** 1.7

Tasks:
- Observe services through labelled Docker objects on request and on an interval, storing observations apart from desired state and deployment results (ADR-0006)
- Implement `ship status PROJECT`, `ship deployments PROJECT` and `ship logs PROJECT[/SERVICE] [--follow]`, each with `-o json` matching its fixture
- Make `ship projects` show each project's environment and overall state
- Report `unknown` for every service when Docker is unreachable, and change nothing
- Add the API routes these commands read
- Add fixture tests so a shape change fails the build

Suggested packages:
- `crates/ship`
- `crates/ship-daemon`

Acceptance criteria:
- [ ] `ship status -o json`, `ship deployments -o json` and `ship logs -o json` match their fixtures apart from ids and timestamps
- [ ] A service with no health check is `running_unverified`, never `healthy`
- [ ] Stopping Docker turns every service `unknown`, and nothing is cleaned up
- [ ] A failed deployment is listed with its failure and leaves `last_deployment` of the running release visible
- [ ] `ship logs --follow` prints lines as they are produced, with stdout and stderr told apart

---

## Workstream 1.12: Lifecycle commands

- **Risk:** high
- **Coder:** claude-code/opus-5-5
- **Reviewer:** codex/gpt-6-astra
- **Stage:** push
- **Blocked by:** 1.11

Tasks:
- Implement `ship start`, `stop`, `restart`, `redeploy` and `remove` for a project or one service, each as an operation
- `stop` sets desired state `stopped`; nothing in Ship starts that environment again until `start` (OVERVIEW §3.3)
- `restart` restarts containers on the current release; `redeploy` deploys the current revision again with current config; neither is the other (ADR-0006 rule 10)
- `remove` stops and removes containers and networks, keeps every named volume and the history, and sets desired state `removed`
- Ask for confirmation before `remove`; never delete a volume
- Record each action in the audit log

Suggested packages:
- `crates/ship`
- `crates/ship-daemon`

Acceptance criteria:
- [ ] A stopped environment stays stopped across a daemon restart and a host reboot
- [ ] `ship status` after `stop` shows `stopped`, not `failed`
- [ ] Named volumes exist, with their data, after `remove`
- [ ] `redeploy` after a config change clears `stale_config`; `restart` does not change the release
- [ ] Each command on an environment with an operation in flight waits its turn

---

## Workstream 1.13: UI for projects, services and logs

- **Risk:** medium
- **Coder:** claude-code/opus-5-5
- **Reviewer:** codex/gpt-6-astra
- **Stage:** push
- **Blocked by:** 1.12

Tasks:
- Add the project list and a project page: services with desired state, last deployment and observed status shown as three things
- Show an operation's progress and log live over SSE, resuming after a dropped connection
- Add deployment history with failures and their phase
- Add logs per service with follow
- Add start, stop, restart, redeploy and remove, with a confirmation naming what will be affected for the destructive ones
- Show config values, and secret names with their timestamps and no value
- Extend the web contract fixtures for every new shape

Suggested packages:
- `web/src`
- `crates/ship-daemon`

Acceptance criteria:
- [ ] Every state the UI shows equals what the API returns, checked in a test
- [ ] The operation view resumes after a dropped connection with no line lost or repeated
- [ ] Destructive actions cannot be triggered without the confirmation
- [ ] Every action is reachable by keyboard
- [ ] No secret value is present in any page or request the UI makes

---

## Workstream 1.14: Recovery and invariants

- **Risk:** high
- **Coder:** claude-code/opus-5-5
- **Reviewer:** codex/gpt-6-astra
- **Stage:** push
- **Blocked by:** 1.12

Tasks:
- On daemon start, rebuild observed state from labelled Docker objects and reconcile nothing (ADR-0006 rule 11)
- Clean staging directories and partial uploads left by an interrupted operation, and nothing else
- Add invariant tests over a full scenario: no known test secret in logs, events, API responses, generated Compose files or image history
- Add an invariant test that every listener is on the unix socket or 127.0.0.1
- Add a test that kills the daemon at each phase of a deployment and checks the previous release still serves
- Update the README, the operator notes and `docs/contracts/cli-json.md` to what was built

Suggested packages:
- `crates/ship-daemon`
- `crates/ship`
- `docs/`

Acceptance criteria:
- [ ] Killing the daemon in any phase leaves the previous release serving and the operation `interrupted`
- [ ] After a restart, status matches what Docker reports with no operator action
- [ ] The invariant tests fail when a secret is deliberately leaked or a listener is deliberately bound elsewhere
- [ ] No object without Ship's labels changed during the whole test scenario

---

## Phase Gate

> This section defines how Phase 1 is validated as a composed system.
> It becomes a `gate` slice automatically (phase-1-gate, risk high,
> blocked by every workstream in the phase).

Composition scenarios to validate:
- [ ] Ship is installed from a release with `install.sh` on two of the maintainer's own machines, as two independent targets
- [ ] A real Dockerfile project and a real multi-service Compose project are pushed from a laptop over ssh and serve on their published ports
- [ ] A database with its data on a host path runs under Ship, and its data is intact after redeploy, stop, start, remove and a host reboot
- [ ] A GPU workload runs under Ship on the machine that has the GPU, after confirmation of its reservation
- [ ] The workloads above run under Ship in place of the orchestrator that ran them before, for at least two days
- [ ] A failed build and a failed health check each leave the previous release serving
- [ ] A user in the `ship` group, with no terminal, runs `ship push --wait -o json` and gets the result line and exit code of the contract; a file needing confirmation exits 3
- [ ] Stopping Docker shows `unknown` everywhere and changes nothing; killing the daemon mid-deploy leaves an `interrupted` operation
- [ ] Containers, networks and volumes that Ship did not create are unchanged on both machines
- [ ] Every `-o json` output matches its recorded fixture, and no test secret appears in any output

Performance / resource checks:
- [ ] A push with no source change reaches `succeeded` in under 30 seconds on a LAN, the build cache being warm
- [ ] Idle daemon resident memory with ten projects is measured on a gate machine and recorded in the README
- [ ] Uploading a 500 MB tree stays under a fixed client and daemon memory bound
