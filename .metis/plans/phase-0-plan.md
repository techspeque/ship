---
type: plan
phase: 0
title: Foundation
overview_ref: OVERVIEW.md §4 Phase 0
status: draft
created: 2026-11-24
---

# Phase 0 — Foundation

> Derived from: OVERVIEW.md §4 Phase 0
> Produces: slices phase-0-ws-0.1 … 0.9 and phase-0-gate in .metis/slices.yaml
> ADRs: 0001, 0002, 0003, 0004, 0012 (all in .metis/adr/)

## Context

Phase 0 builds everything Phase 1 stands on and deploys nothing. After it, a
`ship daemon` runs on a Linux host as a systemd service, holds an empty
registry in SQLite, reports whether Docker and Compose are usable, and
serves a UI shell. A laptop reaches it with `ship target add lab
ssh://user@host` and gets an empty project list back as JSON. No listener
exists outside the unix socket and 127.0.0.1, and Ship has touched nothing
in Docker.

## Dependencies

- Requires: none (first phase)
- External: a Linux host or VM with Docker Engine, the Compose v2 plugin, systemd and sshd for the gate; GitHub Actions for CI
- Local: the gate hosts, their SSH names and what an agent may do on them are described in `.local/hosts.md`, which is gitignored and never committed (ADR-0012 rule 4). Steps that need root on a host are the Human's.

---

## Workstream 0.1: Workspace scaffold and verify gate

- **Risk:** medium
- **Coder:** claude-code/opus-5-5
- **Reviewer:** codex/gpt-6-astra
- **Stage:** foundation

Tasks:
- Create the Cargo workspace with the seven crates in OVERVIEW §2.3, each compiling with a placeholder
- Pin the Rust toolchain in `rust-toolchain.toml` and commit `Cargo.lock`
- Add a `Makefile` whose `verify` target runs `cargo fmt --check`, `cargo clippy --workspace --all-targets -- -D warnings` and `cargo test --workspace`
- Add a test that fails when a leaf crate depends on another leaf crate (the §2.3 dependency rule)
- Add a GitHub Actions workflow running `make verify` on linux amd64
- Add LICENSE (MIT), a README stating pre-alpha status, SECURITY.md with a private reporting route and CONTRIBUTING.md with the build and test commands
- Extend `.gitignore` for `target/` and `web/` build output

Suggested packages:
- `crates/ship`
- `crates/ship-core`
- `crates/ship-daemon`
- `crates/ship-docker`
- `crates/ship-source`
- `crates/ship-store`
- `crates/ship-net`

Acceptance criteria:
- [ ] `make verify` passes on a clean checkout
- [ ] `ship --version` prints a version, commit and platform
- [ ] The dependency-rule test fails when a leaf-to-leaf dependency is added
- [ ] CI is green on the integration branch
- [ ] The repository contains no real hostname, address or personal name

---

## Workstream 0.2: Domain types and state machines

- **Risk:** high
- **Coder:** claude-code/opus-5-5
- **Reviewer:** codex/gpt-6-astra
- **Stage:** foundation
- **Blocked by:** 0.1

Tasks:
- Define typed, prefixed, immutable ids for project, environment, service, revision, release, deployment and operation, with parsing and display
- Define slug validation, and the derivation of a Compose project name from an environment id (ADR-0001 rule 4)
- Define desired state, service status and operation phase enums with their legal transitions (ADR-0006, ADR-0011)
- Define the typed error with stable codes and the `{code, message, details, operation_id}` shape
- Define the API version constant and the request and response types for `/api/v1/host` and `/api/v1/projects`, shared by client and daemon
- Add a redaction type for secret values whose `Debug` and `Display` never print the value

Suggested packages:
- `crates/ship-core`

Acceptance criteria:
- [ ] Ids round-trip through string form and reject a wrong prefix
- [ ] Every illegal state and phase transition is rejected by a test
- [ ] A Compose project name is deterministic for an id and never contains user input
- [ ] Formatting a redacted value in any way does not reveal it
- [ ] `ship-core` depends on no other workspace crate and performs no I/O

---

## Workstream 0.3: SQLite store and migrations

- **Risk:** medium
- **Coder:** claude-code/opus-5-5
- **Reviewer:** codex/gpt-6-astra
- **Stage:** foundation
- **Blocked by:** 0.2

Tasks:
- Add SQLx with SQLite in WAL mode and an in-repo migration runner
- Write the first migration: project, environment, service, revision, release, deployment, operation, operation_event and audit_event (OVERVIEW §2.2)
- Implement repositories for create, get and list on project, environment and service, and create, get and append-event on operation
- Enforce in the schema that revision and release rows cannot be updated
- Refuse to open a database whose schema version is newer than the binary, with a typed error
- Back up the database file before applying a migration to an existing database

Suggested packages:
- `crates/ship-store`

Acceptance criteria:
- [ ] A fresh state directory migrates to the current schema and reopens cleanly
- [ ] An UPDATE on a revision or release row fails
- [ ] Operation events for one operation have gapless, increasing sequence numbers under concurrent appends
- [ ] Opening a newer-schema database fails with the typed error and changes nothing
- [ ] A backup file exists after a migration on an existing database

---

## Workstream 0.4: Daemon skeleton, listeners and logging

- **Risk:** high
- **Coder:** claude-code/opus-5-5
- **Reviewer:** codex/gpt-6-astra
- **Stage:** foundation
- **Blocked by:** 0.3

Tasks:
- Implement `ship daemon [--state DIR]`: open the store, bind listeners, serve Axum, shut down cleanly on SIGTERM
- Bind a unix socket at mode 0660 and a 127.0.0.1 TCP listener; refuse to start if either cannot be bound as specified (ADR-0003)
- Serve `/healthz` with no host details, and `/api/v1/projects` returning the registry
- Reject mutating requests that fail a same-origin check (ADR-0003 rule 6)
- Emit structured JSON logs to stderr through a logger that redacts registered secret values
- Reject `ship daemon` on non-Linux platforms with a clear message (ADR-0002 rule 2)

Suggested packages:
- `crates/ship-daemon`
- `crates/ship-net`

Acceptance criteria:
- [ ] A test starts the daemon, enumerates its listening sockets and fails on any address other than the unix socket and 127.0.0.1
- [ ] The socket file has mode 0660 and a stale socket from a crashed daemon is replaced safely
- [ ] `/healthz` returns 200 and a body containing no hostname, version or path
- [ ] A POST with a foreign `Origin` header is rejected
- [ ] A registered test secret written to the logger does not appear in stderr
- [ ] SIGTERM closes the database and removes the socket

---

## Workstream 0.5: Docker and Compose preflight

- **Risk:** medium
- **Coder:** claude-code/opus-5-5
- **Reviewer:** codex/gpt-6-astra
- **Stage:** foundation
- **Blocked by:** 0.4

Tasks:
- Implement the subprocess runner in `ship-docker`: structured argv only, no shell, bounded output, timeout (ADR-0001 rule 2)
- Detect Docker Engine reachability and version, and the Compose v2 plugin version
- Compare both against a supported range held in one place and tested in CI
- Read host CPU count, memory and free disk in the state directory
- Serve `/api/v1/host` with Docker status (`ready`, `unreachable`, `unsupported_version`), versions and capacity
- Add a CI job that runs these tests against a real Docker Engine

Suggested packages:
- `crates/ship-docker`
- `crates/ship-daemon`

Acceptance criteria:
- [ ] With Docker running, `/api/v1/host` reports `ready` and both versions
- [ ] With the Docker socket unavailable, it reports `unreachable` with an actionable message, and the daemon stays up
- [ ] A version outside the range reports `unsupported_version` and names the range
- [ ] The runner offers no API that accepts a shell string, and a test passes an argument containing shell metacharacters unchanged
- [ ] Preflight creates, changes or removes no Docker object, shown by listing objects before and after

---

## Workstream 0.6: Client transports and targets

- **Risk:** high
- **Coder:** claude-code/opus-5-5
- **Reviewer:** codex/gpt-6-astra
- **Stage:** foundation
- **Blocked by:** 0.4

Tasks:
- Implement `ship target add NAME (ssh://USER@HOST | local)`, `ship target use NAME` and `ship target list`, stored in a 0600 config file
- Implement the local transport: HTTP over the daemon's unix socket
- Implement `ship dial-stdio`: bridge stdin and stdout to the unix socket and exit when either side closes
- Implement the ssh transport: spawn `ssh` with structured argv running `ship dial-stdio`, and speak HTTP over its pipes
- Negotiate the API version on connect and fail with the upgrade instruction when client and daemon are incompatible
- Implement `ship projects` with text and `-o json` output, and record a JSON fixture for the contract (ADR-0004 rule 4)
- Reject `tailnet://` targets with a message that the transport arrives in Phase 2

Suggested packages:
- `crates/ship`
- `crates/ship-net`

Acceptance criteria:
- [ ] `ship projects -o json` over the local socket returns an empty list matching the fixture
- [ ] The same command over `ssh://` against a real sshd in CI returns the same bytes
- [ ] With no daemon running, the client exits non-zero and prints the command that starts one
- [ ] With the host unreachable, the client reports that and does not hang past its timeout
- [ ] A user not in the socket's group gets a permission error that names the group
- [ ] A host name or user containing shell metacharacters reaches `ssh` as one argument or is rejected

---

## Workstream 0.7: UI shell

- **Risk:** medium
- **Coder:** claude-code/opus-5-5
- **Reviewer:** codex/gpt-6-astra
- **Stage:** foundation
- **Blocked by:** 0.5

Tasks:
- Create `web/` with Svelte, TypeScript, Vite and Tailwind, with dependencies pinned and the lockfile committed
- Build one page: host status from `/api/v1/host` and an empty projects list, with loading, error and Docker-unavailable states
- Embed the built assets in the binary and serve them from the daemon with a fallback to the index for client routes
- Generate or validate the TypeScript API types against the Rust types so they cannot drift
- Add `ship open`, which opens the UI directly for a local target and through an ssh-forwarded local port for an ssh target
- Add the web lint, type check, unit tests and build to `make verify`

Suggested packages:
- `web/`
- `crates/ship-daemon`
- `crates/ship`

Acceptance criteria:
- [ ] The release binary serves the UI with no Node.js installed and no request to any other origin
- [ ] The page shows Docker `ready`, `unreachable` and `unsupported_version` distinctly
- [ ] A changed Rust response type without a matching TypeScript change fails `make verify`
- [ ] `ship open` against an ssh target shows the UI on a 127.0.0.1 port and closes the forward when interrupted
- [ ] Light and dark mode render, and the page is operable by keyboard

---

## Workstream 0.8: Service install

- **Risk:** medium
- **Coder:** claude-code/opus-5-5
- **Reviewer:** codex/gpt-6-astra
- **Stage:** foundation
- **Blocked by:** 0.5

Tasks:
- Implement `ship service install`: create the `ship` user and group, the state directory at 0700 and the runtime socket directory, and write the systemd unit
- Give the `ship` user Docker access and print plainly that this is root-equivalent
- Implement `ship service uninstall`: remove the unit and nothing else
- Make both commands idempotent and refuse to overwrite a unit they did not write
- Document adding a login user to the `ship` group for the ssh transport

Suggested packages:
- `crates/ship`

Acceptance criteria:
- [ ] On a clean VM, `ship service install` then `systemctl start ship` yields a running daemon whose socket is group `ship`, mode 0660
- [ ] Running install twice changes nothing the second time
- [ ] The daemon restarts after being killed and after a reboot
- [ ] `ship service uninstall` leaves the state directory, every container and every volume in place
- [ ] No default password, token or key is created

---

## Workstream 0.9: Release pipeline and install script

- **Risk:** medium
- **Coder:** claude-code/opus-5-5
- **Reviewer:** codex/gpt-6-astra
- **Stage:** foundation
- **Blocked by:** 0.7

Tasks:
- Add a release workflow building static binaries for linux amd64 and arm64 and macOS amd64 and arm64, with the UI embedded
- Publish checksums, signatures and an SBOM with each release (ADR-0012 rule 2)
- Write `install.sh`, modelled on Metis's, that verifies checksum and signature before installing and never pipes to a root shell unverified
- Run dependency audit and licence checks in CI for Rust crates and web packages
- List in the README exactly the platforms CI builds and tests

Suggested packages:
- `.github/workflows`
- `scripts/`

Acceptance criteria:
- [ ] A tag produces four archives, a checksum file, signatures and an SBOM
- [ ] `install.sh` refuses an archive whose checksum or signature does not match
- [ ] The linux binaries run on a host with no Rust, Node.js or extra libraries installed
- [ ] The dependency audit fails the build on a known vulnerability
- [ ] The binary makes no network request at start-up or on any Phase 0 command

---

## Phase Gate

> This section defines how Phase 0 is validated as a composed system.
> It becomes a `gate` slice automatically (phase-0-gate, risk high,
> blocked by every workstream in the phase).

Composition scenarios to validate:
- [ ] On a clean Linux VM with Docker, install a release build with `install.sh`, run `ship service install` and start the service; the daemon is up after a reboot
- [ ] From a macOS or Linux laptop, `ship target add lab ssh://user@host` then `ship projects -o json` returns the empty list fixture
- [ ] `ship open` shows the UI with Docker `ready`; stopping Docker changes it to `unreachable` and the CLI reports the same, with the daemon still running
- [ ] Listing listening sockets on the host shows Ship only on its unix socket and 127.0.0.1
- [ ] Containers, networks and volumes that existed before installing Ship are byte-for-byte unchanged in `docker inspect` after the whole scenario
- [ ] A client one API version behind the daemon fails with the upgrade instruction
- [ ] `ship service uninstall` leaves the state directory and all Docker objects in place
- [ ] Every `-o json` output matches its recorded fixture

Performance / resource checks:
- [ ] Idle daemon resident memory and CPU are measured on the gate VM and recorded in the README
- [ ] `ship projects` over ssh completes in under two seconds on a LAN, excluding the SSH handshake time reported separately
