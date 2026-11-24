---
type: overview
project: ship
status: living
last_updated: 2026-11-24
---

# Ship — Full Specification

> Open-source, self-hosted: one binary that turns a folder on your laptop into a running, observable application on a Linux machine you own, without Kubernetes, a cluster or a hosted account.

Supersedes the v0.4 prose draft, kept for reference in
`docs/archive/overview-v0.4.md`. Where the two disagree, this document and
the ADRs in `.metis/adr/` win.

---

## 1. Purpose

Between `docker compose up` on a server and a full container platform there
is a gap. Filling it today means SSH, hand-edited Compose files, guessing
which container is which, and no record of what was deployed when.

Ship has two goals:

1. **One command from working tree to running app.** `ship push .` sends the
   current directory, uncommitted changes included, to a Ship host, builds
   it, starts it and prints the real address. No Git round-trip, no registry.
2. **Operate applications, not containers.** Projects, environments,
   services, routes and releases are first-class. Ship records what you asked
   for, what the last deployment did and what Docker reports now, as three
   separate things, and never shows one as another (ADR-0006).

Ship is the third tool in a family: Metis plans and reviews work, Gaia runs
coding-agent sessions on your machines, Ship runs what gets built. Ship is
built **first**, because it is needed now to host personal projects. So it
must stand alone completely, and it carries the seams the other two will use
later: the same network model as Gaia (ADR-0003) and a versioned JSON CLI
contract (ADR-0004). Ship never needs Gaia or Metis; neither needs Ship.

Ship is MIT licensed, self-hosted, with no hosted service and no telemetry
(ADR-0012). Each installation belongs to one trusted operator.

Users: one developer hosting their own projects on one machine, first.
Several machines (independent Ship hosts) and deployments shown in Gaia, second;
they shape the seams but not Phase 0–1 scope.

---

## 2. Architecture

```
   you (laptop)                         you (browser, any tailnet device)
   ship push .                                      │
        │ ssh (dial-stdio)  or  tailnet             │ tailnet
        v                                           v
┌──────────────────────────────────────────────────────────────┐
│ ship daemon  (Linux host)                                     │
│  unix socket · 127.0.0.1 · optional tailnet-interface listener│
│  API · UI · operation queue · reconciler · SQLite             │
└───────────────┬──────────────────────────────────────────────┘
                │ docker / docker compose subprocesses
                v
   Docker Engine + Compose v2  (images, containers, networks, volumes)
                │
                v
   your apps:  host:port  ·  tailnet route  ·  optional Caddy route
```

One binary, `ship`. `ship daemon` is the long-running server on a Linux
host; every other subcommand is the client (ADR-0002). The client reaches a
daemon three ways, none of which is a listener Ship secures itself
(ADR-0003):

- **local:** the daemon's unix socket, when client and daemon share a machine;
- **ssh:** `ssh host ship dial-stdio`, which bridges to that same socket;
- **tailnet:** the daemon's listener on the host's tailnet address, authorised
  by tailnet identity.

### 2.1 System Boundaries

Inside: source snapshots, the project/environment/service registry, build and
deploy orchestration, durable operations and their logs, config and secret
storage, route records, state observation and reconciliation, the UI and CLI.

Outside, consumed as-is:

| External | How Ship uses it |
|---|---|
| Docker Engine, Compose v2, BuildKit | Run as subprocesses with structured argv. Ship builds no images and schedules no containers itself (ADR-0001). |
| OpenSSH | Transport for the client when there is no tailnet. Ship adds no auth of its own on top (ADR-0003). |
| Tailscale / Headscale (system `tailscaled`) | Optional. Tailnet listener, peer identity through the local API, and tailnet routes for apps (ADR-0003, ADR-0010). |
| Caddy | Optional HTTP ingress for hostname routes (ADR-0010). |
| Git remotes, OCI registries | Later sources. Fetched, never trusted because of where they came from. |
| Gaia | Absent by default. A Gaia node reads `ship … -o json` to show deployments beside sessions (ADR-0004, Gaia ADR-0013). |
| Metis | Drives development of this repository. No runtime relationship. |

### 2.2 Data Model

SQLite, one file (ADR-0002):

| Entity | Key fields |
|---|---|
| Project | id, slug, created_at |
| Environment | id, project, name, desired state (`running` \| `stopped` \| `removed`), compose project name (derived from id), recovery policy |
| Service | id, environment, name, kind (`web` \| `worker` \| `job` \| `compose`), port, health config |
| Revision | id, project, kind (`snapshot` \| `git` \| `image`), content hash or commit or digest, manifest fingerprint. Immutable. |
| Release | id, revision, image ids/digests, effective non-secret config fingerprint. Immutable. |
| Deployment | id, environment, release, operation, result, previous release, started_at, ended_at |
| Operation | id, idempotency key, kind, phase, status, typed failure, log cursor |
| OperationEvent | operation, sequence, type, phase, message, ts |
| Route | id, environment, service, provider (`port` \| `tailnet` \| `caddy`), address, status |
| ConfigValue | scope (project \| environment \| service), key, value. Non-secret only. |
| Secret | scope, name, backend ref, updated_at. Never the value. |
| Observation | service, container state, health, restarts, observed_at |
| AuditEvent | actor, action, target, ts |

Phase 3 adds Resource and Attachment. On disk beside the database: revision
snapshots, operation logs, the secret store, generated runtime definitions.

### 2.3 Component Structure

```
crates/ship           binary: argument parsing, client commands, `daemon` dispatch
crates/ship-core      domain types, state machines, config precedence, trust profile
crates/ship-daemon    Axum API, operation queue, reconciler, UI serving
crates/ship-docker    docker / compose subprocess adapter, inspect, events
crates/ship-source    snapshot pack and safe unpack; later git and image sources
crates/ship-store     SQLite access (SQLx) and migrations
crates/ship-net       listeners, ssh dial-stdio, tailscaled local-API client
web/                  Svelte + TypeScript UI, built to static files, embedded
```

Dependency rule: `ship` and `ship-daemon` depend on the leaves; leaves depend
only on `ship-core` and never on each other. All Docker access lives in
`ship-docker`; all tailnet access in `ship-net`.

---

## 3. Constraints

### 3.1 Technology

- **Language:** Rust (ADR-0002)
- **Server:** Axum + Tokio; SQLite through SQLx, WAL mode, migrations in-repo
- **Client:** Clap; one static binary for linux amd64/arm64 and macOS amd64/arm64. `ship daemon` is supported on Linux only.
- **UI:** Svelte + TypeScript + Vite + Tailwind, embedded in the binary; no Node.js at runtime
- **Runtime:** Docker Engine + Compose v2 plugin on the host; a systemd unit runs the daemon as a dedicated `ship` user with Docker access
- **Networking:** no TLS, pairing or token code of Ship's own (ADR-0003)

### 3.2 Non-Goals (do NOT build)

- Multi-host orchestration: scheduling, clustering, HA, consensus, service mesh, cross-host migration.
- A container runtime, image builder, buildpack system or OCI registry.
- A scheduler other than Docker's restart policies and one small reconciler.
- Multi-tenant hosting, team RBAC, billing, or running code for anyone but the operator.
- A sandbox. A Dockerfile or Compose file Ship accepts can take over the host.
- Transport security of Ship's own: no pairing codes, certificates, pinning or bearer tokens.
- Any listener on 0.0.0.0, or any public exposure Ship turns on by itself.
- Embedding a tailnet client. Ship uses the host's `tailscaled` or nothing.
- A secrets vault, APM product, firewall or backup system.
- Importing Gaia or Metis code, or requiring either to be installed.
- Podman, Swarm, Nomad, Kubernetes or Windows hosts.
- Zero-downtime deploys, database rollback, or automatic backups.

### 3.3 Invariants (must ALWAYS hold)

- Every listener binds to a user/group-only unix socket, 127.0.0.1 or the host's tailnet address. Never 0.0.0.0.
- No secret value appears in a log, an operation event, the UI, an API response, an image, a build argument or a persisted Compose file.
- A snapshot never contains `.git/`, `.env`, `.env.*` (other than `.env.example`-style templates) or private keys unless the operator names them explicitly.
- Unpacking a snapshot never writes outside its own new staging directory.
- A revision and a release are never modified after creation.
- Ship never runs `down -v` and never deletes a named volume except through a separately confirmed, volume-specific action.
- A failed upload, build or deployment leaves the previous release, its containers and its data as they were.
- Desired state, last deployment result and observed state are stored and shown separately. A service is `healthy` only when a health check says so; otherwise `running_unverified`.
- Docker unreachable is `unknown`, never `stopped`, and never triggers cleanup.
- Ship touches only Docker objects carrying its own labels, or ones explicitly adopted.
- An environment stopped through Ship is never restarted by the reconciler.
- Every mutation is a persisted operation before any side effect, and mutations on one environment never run concurrently.
- Docker and Compose are invoked with structured argv. Never through a shell, never with user-supplied flags.
- The CLI never prints an address that does not exist. With no route it prints `Endpoint: not configured`.
- Ship works with no tailnet, no Gaia and no Metis present.

---

## 4. Phases (High-Level Roadmap)

### Phase 0 — Foundation
Workspace, CI, release pipeline, open-source scaffolding (LICENSE,
CONTRIBUTING, SECURITY). `ship daemon` skeleton, unix socket, client over the
local socket and over `ssh … ship dial-stdio`, SQLite schema and migrations
for project / environment / service / revision / release / deployment /
operation, Docker and Compose preflight, UI shell, systemd unit.

### Phase 1 — Push
The first usable release, and the one that hosts personal projects.
Snapshot pack, upload and safe unpack; Dockerfile and Compose detection;
Dockerfile build into a generated single-service Compose project; Compose
import with several services under the trust profile (ADR-0008); published
host:port; operation queue with streamed, resumable logs; config values and
write-only secrets injected at deploy; start / stop / restart / redeploy /
remove; deployment history; a UI showing projects, services, state and logs;
recovery of interrupted operations on daemon start. One environment,
`development`, per project.

The phase is not done on a test VM. Its gate is adoption: Ship installed on
the maintainer's own machines, as two independent targets, hosting real
Dockerfile and Compose projects, a database on a host path and a GPU
workload, in place of the orchestrator that ran them before. Host names,
addresses and the migration runbook stay out of this repository (ADR-0012).

### Phase 2 — Compose, Environments and Tailnet
Named environments beyond `development`, with the port and
`container_name` conflicts that brings (ADR-0008), encrypted secret store, tailnet listener with peer-identity authorisation, tailnet and Caddy
route providers, observe-only reconciler with drift shown in the UI.

### Phase 3 — Resources and Releases
PostgreSQL and Redis templates, explicit attachments, retained-image
rollback without rebuild, quotas, retention and garbage collection,
safe-drift repair as an opt-in recovery policy.

### Phase 4 — Git and Image Sources
Public Git import with resolved commit, prebuilt image deploys by digest,
private repositories and registries through scoped credentials, opt-in
webhooks that never deploy without approval.

### Phase 5 — Gaia and Release
`ship … -o json` contract frozen at v1 and consumed by Gaia's adapter
(Gaia ADR-0013), installers, operator docs, threat model document,
upgrade path, v1.0.

---

## 5. Security Model

Trust boundaries, outermost first:

1. **Network.** Ship has no public listener. The daemon is reachable only through its unix socket, 127.0.0.1, or the host's tailnet address. Exposing an app or the UI to the internet is something the operator does deliberately with other tools.
2. **Tailnet (optional).** Only tailnet devices reach the tailnet listener. Each request is authorised by peer identity from `tailscaled` plus a host-side allowlist. Tailnet ACLs apply before that.
3. **SSH.** Whoever can SSH to the host as a member of the `ship` group can operate Ship. That is the same trust as shell access to a machine that runs Docker.
4. **Unix socket.** Mode 0660, group `ship`. Membership is equivalent to Docker access, which is equivalent to root on the host.
5. **Daemon.** Holds the secret store and drives Docker. Compromise of the daemon is compromise of the host.
6. **Source and workloads.** Trusted because the operator pushed them, but treated as dangerous: a build or a container can read what it is given and, with a permissive Compose file, much more. The trust profile catches accidents, not attackers.

Consequences stated plainly:

- Ship is not an isolation boundary. Do not use it to run code from people you do not trust.
- A running service can read every secret injected into it.
- The UI on the tailnet has no login page; identity is the tailnet's. Mutating requests still require a same-origin check, because a web page open in a tailnet device's browser can otherwise reach the daemon.
- Sensitive data: secrets (high), snapshots and build logs (medium: source and whatever the build printed), state and history (low).
- A Gaia session can call `ship push` exactly when the OS user running that Gaia node is in the `ship` group. Gaia does not sandbox sessions, so group membership is the only gate, and granting it lets agents run containers on that machine (ADR-0004).
- Vulnerabilities are reported privately under SECURITY.md; releases carry checksums, signatures and an SBOM (ADR-0012).

---

## 6. API / Interface Contracts

### CLI (human)

```
ship daemon [--state DIR] [--tailnet]
ship target add NAME (ssh://USER@HOST | tailnet://HOST | local)
ship target use NAME
ship push [PATH] [--project P] [--env E] [--file F] [--port N]
          (--wait | --detach) [--dry-run] [--to TARGET]
ship projects
ship status PROJECT [--env E]
ship deployments PROJECT [--env E]
ship logs PROJECT[/SERVICE] [--env E] [--follow]
ship start|stop|restart|redeploy|remove PROJECT[/SERVICE] [--env E]
ship vars set|unset|list KEY[=VALUE] --project P [--env E]
ship secrets set|unset|list NAME (--prompt | --stdin) --project P [--env E]
ship open PROJECT
ship dial-stdio                       # internal: ssh transport endpoint
```

Every list and status command supports `-o json`. Secret values are never
accepted as arguments. `--wait` streams ordered progress and exits non-zero
when the deployment fails.

Later phases add `ship env`, `ship routes`, `ship rollback`, `ship add`,
`ship resource attach` and `ship deploy <git-url | image:ref>`.

### Daemon HTTP API

JSON over HTTP on the unix socket, 127.0.0.1 and the tailnet listener,
versioned under `/api/v1`. Resources: host, projects, environments, services,
uploads, deployments, operations, config, secrets (metadata only), routes.
Mutations take an idempotency key and return `202` with an operation id.
Operation events stream over SSE at `/api/v1/operations/{id}/events` and
resume from a sequence number. Errors are
`{ "code", "message", "details", "operation_id" }` with stable codes.
`/healthz` reveals nothing about the host.

### JSON contract (provided to Gaia and scripts, ADR-0004)

`ship push --wait`, `ship projects`, `ship status`, `ship deployments` and
`ship logs`, all with `-o json`. `ship push --wait` behaves like a local
build tool: streamed progress, exit code 0 only when the deployment met its
verification criteria, a reserved documented range for Ship's own failures.
Shapes are provisional until Phase 5, then change only with a major version.

### Docker labels

Every object Ship creates carries `dev.ship.project`, `dev.ship.environment`,
`dev.ship.service` and `dev.ship.release` with immutable ids. These labels
are how Ship recognises its own objects after a restart.

---

## 7. Testing Strategy

- **Unit:** state transitions, config precedence, idempotency, slug and id parsing, trust-profile decisions, secret redaction.
- **Snapshot:** ignore rules, uncommitted-file capture, default secret exclusions, path traversal, absolute paths, links, device entries, Unicode and case collisions, oversize and decompression bombs, digest mismatch; property and fuzz tests on the unpacker.
- **Docker:** a real disposable Docker Engine in CI. Dockerfile build, multi-service Compose import, each trust-profile outcome, build failure, port conflict, failing health check, volume preserved across redeploy and remove, daemon killed mid-operation. A mocked Docker passing is never acceptance.
- **Transport:** local socket permissions, `dial-stdio` over a real sshd in CI, tailnet authorisation against a fake `tailscaled` local API; no Tailscale account in CI.
- **Reconciler:** intentional stop stays stopped, Docker unreachable changes nothing, unlabelled objects untouched, replayed operation creates no second deployment.
- **Contract:** recorded JSON fixtures for every `-o json` command; a shape change fails the build.
- **Invariant tests:** grep logs, events, API responses, generated Compose files and image history for known test secrets; scan for any listener not on the allowed addresses; fail on any hit.
- **UI:** state shown matches the API, SSE resume, destructive confirmations, keyboard access.

---

## 8. Operational Concerns

- **Install:** `install.sh` as Metis does, verifying checksums and signatures; `ship service install` writes the systemd unit and creates the `ship` user and group.
- **First run:** `ship daemon` alone serves the unix socket and 127.0.0.1. Add a laptop with `ship target add lab ssh://you@host`. `ship daemon --tailnet` adds the tailnet listener once the host is on a tailnet.
- **State:** `/var/lib/ship` holds SQLite, snapshots, operation logs, the secret store and generated definitions; back it up as one directory. It is not a backup of application volumes.
- **Logging:** structured JSON to stderr with secret redaction at the logger.
- **Upgrades:** client and daemon negotiate the API version on connect; an incompatible pair fails with the command to fix it. Migrations back up the database first.
- **Coexistence:** Ship needs Docker Engine and installs beside whatever else the host runs. It does not detect, migrate or remove another orchestrator or container engine; replacing one is the operator's runbook, and containers it left behind are unlabelled and so untouched.
- **Several machines:** each is its own installation and its own target. A project is pushed to a named target; nothing moves between them.
- **Uninstall:** removes the binary and unit. Never removes containers, volumes or the state directory.
- **Limits:** one build at a time by default; upload size, file count, build timeout and a disk watermark are configurable with conservative defaults.
- **Failure modes:** daemon down → apps keep running under Docker, nothing new deploys; Docker down → state shows `unknown`, operations fail with a clear error; host suspended or off → the client reports the host unreachable and nothing is assumed to have continued.
- **Open before public release:** whether the name "Ship" is free to use.
