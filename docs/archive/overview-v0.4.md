# Ship - Product Overview and Technical Specification

**Status:** Open-source product and implementation specification (v0.4 draft)  
**Updated:** 24 November 2026  
**Project name:** Ship (working name; public name/brand availability to be verified)  
**Server target:** Single supported Linux host with local Docker Engine and Docker Compose v2  
**Clients:** macOS/Linux CLI and responsive browser UI; Linux amd64/arm64 server builds subject to CI verification  
**Reference stack:** Rust + Axum + Tokio + SQLite; Svelte + TypeScript + Tailwind; Docker Engine + Docker Compose v2; optional Caddy ingress

> **One sentence:** Ship is a lightweight, local-first, self-hosted Platform as a Service (PaaS) that turns a local source directory, Git repository or prebuilt container image into managed applications with services, environments, configuration, networking, deployments and observability, without Kubernetes, Nomad or a cluster.

**Product principle:** *Source in -> Build -> Release -> Deploy -> Register -> Observe -> Reconcile.*

**Important:** This version supersedes prior personal-host-specific drafts. P0/P1/P2 indicate product priority, **not** that all functionality must appear in the first release. Follow the staged delivery plan in Section 12. All CLI commands, API shapes, benchmarks and UI examples are proposed targets, not existing implementation claims. The project is intended for a public open-source repository; license selection and trademark clearance are explicit pre-release decisions.

---

## 1. Product vision and positioning

Ship should feel like a small, local-first PaaS, not simply a graphical wrapper around `docker compose`. It provides a cohesive application-management experience atop Docker, while remaining easy to install, understand, operate and uninstall.

A developer runs `ship push .` from a supported workstation to upload the **current working tree including uncommitted changes** to a self-managed Linux host and receives the actual application endpoint, if one is configured, and a link to the web UI. Alternatively, Ship imports a Git repository or deploys a prebuilt OCI image. The same management interface handles configuration, multiple services, backing resources, logs, health, history and lifecycle operations.

**Unique combination:** direct folder push over an authenticated network connection + Git sources + PaaS-grade app management + low operational footprint. Ship's differentiation is experience and simplicity, not a new scheduler or container runtime.

### Why Ship exists

Ship addresses a precise gap between `docker compose up` and fully fledged container platforms:

- **From any supported development workstation, deploy the current working tree:** `ship push .` transports source over an authenticated connection, including local changes that have not been committed. No Git hosting round-trip is required.
- **From Git, discover and import:** Import a supported Git repository, detect Dockerfile/Compose candidates, review and explicitly deploy them into the same catalogue as direct pushes. GitHub-specific account discovery is an optional provider integration, not a mandatory service.
- **Operate applications, not containers:** Use projects, environments, services, routes, backing resources and immutable release history, without exposing a cluster scheduler to the user.
- **Make the host disposable, not application data:** Rebuild Ship's management plane and reattach existing Docker volumes without unintentional data loss. Backups remain the owner's responsibility unless explicitly implemented.

### Product goals

1. **One-command deployment:** Go from source to a running application with as few prerequisites and prompts as practical.
2. **First-class PaaS concepts:** Treat projects, environments, services, deployments, resources and routes as managed objects, not just Docker containers.
3. **Visual control plane:** Show what is deployed, how it is related, what changed, whether it is healthy and how to fix it.
4. **Local-first and self-hosted:** A complete core experience on one operator-controlled Linux host, reachable over localhost, LAN, VPN or properly secured remote network, without a mandatory third-party SaaS account.
5. **Predictable automation:** Reconcile desired and observed state, prevent conflicting updates, and report meaningful errors without surprising restarts or deletion.
6. **Reproducible releases:** Track immutable source revisions and, as the product matures, retained image/configuration releases for reproducible rollback.
7. **Resource frugality:** Run on commodity Linux systems without dedicated control-plane infrastructure. Publish measured resource requirements and benchmarks, rather than assuming a particular device.

### Primary user stories

- As a developer on macOS or Linux, I can run `ship push .` to deploy my local working tree to an authenticated Ship host, without pushing to Git.
- I can deploy a public Git repository, later an authenticated private Git repository, or an OCI image through CLI/UI.
- I can create a **project** with production, staging or preview **environments**, each with independently configured **services**.
- I can deploy an HTTP app and receive a usable URL without manually hunting for containers or exposing unrelated services.
- I can add a PostgreSQL or Redis resource with a template, attach it to an application, and inspect its storage and connection status.
- I can set environment variables and secrets without packing credentials into my source ZIP.
- I can watch a deployment move from source fetch through build, release, startup and health verification.
- I can start, stop, restart, redeploy, inspect and roll back a service/environment, with persistent volumes preserved.
- I can see a service canvas of declared dependencies and routes without claiming the platform automatically knows every runtime dependency.
- I can shut down Ship's host and resume later without losing deployment records or corrupting its desired state.

### Non-goals and scope boundaries

- **No multi-host orchestration** in the initial product: no cluster membership, distributed scheduling, HA, leader election, consensus, service mesh or cross-node migration.
- No Kubernetes, Nomad, Docker Swarm, general-purpose buildpack ecosystem or bespoke container runtime.
- No public multi-tenant platform, billing, customer accounts, team RBAC or hosting unknown users' untrusted code.
- No arbitrary provider-wide scans or automatic execution of discovered repositories by default.
- No general secrets vault, network firewall replacement, full APM product or custom OCI registry.
- No guarantee of zero-downtime updates, exact database rollback, strong malicious-code isolation or public domain HTTPS in the first milestone.
- No mandatory telemetry service, managed cloud dependency, external PostgreSQL/NATS or background agent fleet.

Ship initially supports **a single trusted operator**; future limited team access requires explicit identity and authorisation design. It is not a secure execution environment for mutually untrusted tenants. Running a Docker build or accepting a privileged Compose specification can grant substantial host access. Security must be explicit rather than implied by a friendly UI.

## 2. Core PaaS domain model

**Model these concepts from the outset**, even when their associated features arrive in later milestones. Avoid premature generic abstractions.

| Concept | Meaning | Important semantics |
| --- | --- | --- |
| **Host** | Local Docker Engine managed by one Ship daemon | One host per Ship installation; CLI may know multiple independent targets, but Ship does not schedule between them |
| **Project** | Logical application or related collection, e.g. `example-app` | Owns environments, services, resources, access policies and history |
| **Environment** | Isolated instantiation of project configuration, e.g. `development`, `staging`, `production`, preview | Stable identity, independent desired state, routes, network, config and deployment; `production` is not automatically public |
| **Service** | Runnable app, HTTP API, worker, job or imported Compose service | Belongs to one environment; has source/image, runtime settings, health and endpoints |
| **Resource** | Managed backing dependency, e.g. PostgreSQL, Redis or persistent disk | Template version, config, persistent volumes, ownership, health, attachment metadata and backup limitations |
| **Source** | Local ZIP, Git URL/ref or OCI image reference | Discovery does not imply trust or deployment |
| **Revision** | Immutable captured source content or resolved immutable image identity | Archive SHA-256 or Git SHA; record build inputs and manifest fingerprint |
| **Release** | Built/selected image artifact plus resolved non-secret runtime definition | Stable identity; image IDs/digests recorded where supported; no secret values in metadata |
| **Deployment** | Attempt to apply a release to an environment/service | Ordered lifecycle, result, health verification, actor, timestamps, logs, previous release reference |
| **Route / Endpoint** | HTTP hostname/path or directly published host:port | Explicit exposure, target service/port, current status, ingress config |
| **Operation** | Durable asynchronous request | Idempotency key, queue state, phase, progress, typed failure, log cursor |
| **Desired state** | Owner-requested runtime state and release/config revision | Separate from last deployment result and actual Docker observations |
| **Observed state** | What Docker actually reports | Includes health, restarts, resources, unknown/unreachable conditions |
| **Attachment** | Declared resource-to-service connection | Config generation is explicit, not hidden application mutation |

### Hierarchy

```text
Host (one Docker Engine)
  Project: example-app
    Environment: development
      Service: web
      Service: api
      Resource: postgres
      Route: app.dev.<configured-domain>
    Environment: staging
      Service: web
      Service: api
      Resource: postgres (separate volume by default)
      Route: app.staging.<configured-domain>

  Project: sample-worker
    Environment: development
      Service: worker
      Route: none (background service)
```

**Defaults:** Create one `development` environment on first import. A Dockerfile source generates one service. Compose import creates one environment containing multiple discovered services; database-like Compose services are still imported services unless the owner explicitly adopts them as managed resource templates. Each environment gets its own stable Compose project identity, network(s), naming scope and runtime data. Do not share a volume between environments implicitly.

### Identifiers and relationships

- Use generated immutable internal IDs for projects, environments, services, releases and resources; show human-friendly slugs separately.
- Use a deterministic, collision-safe Compose project name derived from an immutable environment ID, never from unchecked user input.
- Keep source/revision/release/deployment records separate. Multiple deployments may reuse the same release.
- Record declared service dependencies from Compose and explicit resource attachments. Never infer database dependencies or application topology solely from container names.
- Persistent volumes must have ownership/adoption metadata; deletion is a separate confirmed action.
- Track **desired**, **last deployment**, and **observed** states separately and present disagreements honestly.

## 3. Developer experience and CLI specification

One Rust CLI using Clap + Reqwest. Commands below establish the intended UX; standardise names during implementation and publish stable help/docs.

```bash
# Register and pair an authorised Ship host
ship target add lab https://ship-host.example.test:9443
ship pair lab                         # short-lived pairing code, certificate verification
ship target use lab

# Push the local working tree, including uncommitted changes
cd ./example-app
ship push .
ship push . --project example-app --env development --wait
ship push . --dry-run                # files, exclusions, source type, estimated bytes
ship push . --to lab

# Import a Git repository; provider-specific integration is optional
ship deploy https://github.com/example-org/example-app.git --ref main --project example-app
ship deploy https://git.example.org/team/example-app.git --subdir services/web --env staging

# Import a prebuilt OCI image (later milestone)
ship deploy image:registry.example.org/team/example-app@sha256:<digest> --project example-app

# Project and environment management
ship projects
ship status example-app
ship env ls --project example-app
ship env create staging --project example-app
ship services --project example-app --env staging

# Deployment lifecycle
ship logs example-app/web --env development --follow
ship deployments example-app
ship stop example-app --env development
ship start example-app --env development
ship restart example-app/web --env development
ship redeploy example-app --env development
ship rollback example-app --env development --to <release-id>
ship remove example-app --env development   # preserve persistent data by default

# Backing resources (later milestone)
ship add postgres --project example-app --env development
ship resources --project example-app --env development
ship resource attach postgres --service api --project example-app --env development

# Variables and secrets; do not put secret values in shell arguments
ship vars set LOG_LEVEL=debug --project example-app --env staging
ship secrets set API_KEY --prompt --project example-app --env staging
ship secrets set API_KEY --stdin --project example-app --env staging

# Open the project page or an available app endpoint
ship open example-app
```

### Example deployment result (illustrative CLI output)

```text
$ ship push . --project example-app --env development --wait
Target       lab
Project      example-app
Environment  development
Source       local-snapshot:sha256:...
Detected     compose.yaml (web, api)
Deployment   dep_01...
Status       healthy (web), running_unverified (api)
URL          http://app.dev.example.test  [only with configured DNS/routing]
Fallback     http://<host-ip>:<configured-published-port>
Dashboard    https://ship-host.example.test:9443/projects/example-app
```

Never print a fabricated hostname or port. If no ingress or port publishing is configured, the CLI must say `Endpoint: not configured` and explain how to configure one. A project may have workers or private services with no externally reachable URL.

### Behaviour and ergonomics

- `ship push .` selects the default target, existing project in current CLI metadata if known, otherwise proposes a slug and `development` environment. It pushes the current working tree, not just the last Git commit. A directory without Git metadata works as well.
- Provide `--project`, `--env`, `--file`, `--subdir`, `--port`, `--to`, `--wait`, `--detach`, `--json` and `--dry-run` where applicable.
- An ambiguous Compose/Dockerfile selection prompts interactively; noninteractive use returns a typed error requiring `--file`.
- Use an operation ID for every mutation. `--wait` streams ordered progress and returns a nonzero exit code for failed deployments.
- Success prints target, project/environment, revision, release, services, deployed URLs, health result and management URL.
- Failure prints the phase, actionable error and log location. Never claim health from a successful CLI subprocess alone.
- CLI target credentials are stored in OS-appropriate protected storage, or a permission-restricted config file when secure keychain integration is unavailable.
- The CLI is a thin client, not a second source of authoritative deployment state.

## 4. High-level architecture

```text
                   macOS / Linux CLI                     Git/OCI sources
                       ship push .                   Git import / image pull
                           |                                  |
                   authenticated HTTPS                 controlled fetch
                           +----------------+-----------------+
                                            |
                                            v
+--------------------------------------------------------------------------+
|                     SHIP CONTROL PLANE (RUST DAEMON)                      |
|                                                                          |
| Axum API + session/pairing auth + embedded Svelte frontend               |
| Project/Environment/Service/Resource Registry + SQLite migrations       |
| Source adapters: bounded ZIP upload / Git fetch / OCI reference          |
| Manifest detector + policy validator + BuildKit/Compose adapter         |
| Durable operation queue + per-environment locking + build limiter       |
| Release/deployment manager + desired-state reconciler                   |
| Log/event stream (SSE) + metrics/health poller + audit records          |
| Secret/config resolver + route/resource managers (phased capabilities)  |
+--------------------------------------------------------------------------+
        |                   |                     |
        v                   v                     v
 SQLite + source       Docker Engine          Optional Caddy
 snapshots             + Compose v2           ingress
                      images / containers     private HTTP routes
                      networks / volumes
                              |
                              v
                   Managed app services/resources
```

### Technology choices

| Component | Choice | Rationale |
| --- | --- | --- |
| Control plane | Rust + Axum + Tokio | One native server process; efficient I/O and typed domain models |
| CLI | Rust + Clap + Reqwest | Portable macOS/Linux binary, robust streaming uploads |
| Persistence | SQLite + SQLx migrations + WAL | Transactional local metadata without a database service |
| Frontend | Svelte + TypeScript + Vite + Tailwind | Polished reactive UI, compiled assets embedded into Rust server |
| Container runtime | Docker Engine + Compose v2 | Dockerfile/Compose compatibility, networks, volumes and container lifecycle |
| Image build | Docker BuildKit, invoked through Docker/Compose | Avoid inventing a builder or registry |
| Runtime adapter | Structured Docker CLI/Compose subprocesses first | No shell interpolation; consistent native Docker semantics |
| Reverse proxy | Optional single Caddy instance (introduced as core UX matures) | Managed HTTP routing; avoid reimplementing an HTTP proxy in Rust |
| Operations/events | Durable SQLite operation state, on-disk logs, SSE | No Redis, NATS or external message queue |
| Host service | systemd reference unit | Linux daemon startup, restart and logging; installers may support other init systems later |

**Lightweight design constraint:** The management plane remains one Rust daemon, one SQLite file, compiled UI assets, and Docker. An optional Caddy instance is the only planned always-on adjunct. Application databases are user-provisioned resources, not dependencies of Ship itself.

**Container runtime scope:** Ship initially targets Docker Engine and the Docker Compose v2 plugin. Podman and other runtimes are not promised. Preflight checks must detect unavailable Docker services, incompatible Compose versions, existing ports and resources, and must not modify unrelated software or workloads.

### Single-host deployment boundary

The `ship-server` daemon and its SQLite database live **on a supported Linux host**, separate from the CLI process. The Rust CLI talks to its authenticated API. The daemon may delegate build and runtime operations to local Docker Engine / Compose; it is never a remote agent that schedules onto an arbitrary pool.

A host may be offline, suspended or network-unreachable. CLI operations must fail with an actionable connectivity error; Ship does not promise wake-on-LAN, execution while powered off, or uninterrupted uptime. On next boot, it inspects labelled Docker objects and reconciles conservatively. A CLI may target multiple **independent** Ship installations, but there is no cross-host scheduling or state replication.

### Logical modules / Rust workspace

```text
ship/
  crates/
    ship-server/           # Axum API, auth, SSE and static frontend
    ship-core/             # domain model, policies, lifecycle services
    ship-runtime-docker/   # build, Compose, inspect, events, resource discovery
    ship-sources/          # ZIP, Git, OCI source adapters
    ship-storage/          # SQLite repositories and migrations
    ship-cli/              # macOS/Linux CLI
  web/                     # Svelte + TypeScript UI
  migrations/
  docs/adr/
  tests/                    # integration/e2e fixtures, test Compose apps
```

Do not split the daemon into microservices or add extensible plugin frameworks without a second real use case.

## 5. Ingestion, source detection and releases

### FR-01. Direct source push via ZIP (P0, first vertical slice)

1. Identify project root from `ship push <path>` and detect supported deployment files.
2. Package the **working directory snapshot** including tracked/untracked/uncommitted non-ignored files. Respect `.gitignore` when appropriate plus `.shipignore`.
3. Apply safe default exclusions: `.git/`, `.env`, `.env.*` (except deliberately non-secret templates such as `.env.example`), private keys, `node_modules/`, Rust `target/`, `.next/`, caches and credential stores. Never include known sensitive files merely because Git tracks them. Preserve `.dockerignore` as a file; Docker still applies its own context rules.
4. `--dry-run` reports file count, size estimate, detected definition and excluded sensitive files. Reject accidental home-directory/very large uploads.
5. Create a ZIP and stream it via authenticated HTTPS using bounded memory. The server stores it in a temporary file, validates SHA-256 and checks compressed/extracted size, file count and extraction limits.
6. Reject absolute paths, `..` traversal, path overlaps, symlinks, hardlinks, device entries, unsafe permissions and paths that escape the staging directory. Do not follow source symlinks while packing.
7. Extract safely into a new staging directory; atomically promote it to an immutable revision after validation. No operation may overwrite another revision or arbitrary host files.
8. Import/reuse the project and target environment; queue a deployment using the common pipeline.
9. Retain source fingerprints and optionally display non-authoritative local Git SHA/dirty flag. A push does not require a Git commit.
10. Failed transfer, validation or build must not delete the existing healthy release or persistent resources.

### FR-02. Git import and optional provider discovery (P1)

- Accept HTTPS repository URL, ref and optional monorepo subdirectory. Resolve and record commit SHA.
- Start with public Git repositories over HTTPS. Support private repositories later through credential helpers or narrowly scoped provider integrations. GitHub Apps may offer selected-repository, read-only access with short-lived tokens; no public release may require a GitHub account for direct push or public Git import. Never place credentials in command arguments, URLs or logs.
- Import the same manifest model as ZIP pushes; do not create a parallel build path.
- The repository catalogue may inspect **explicitly authorised** provider repositories for root or selected subdirectory Dockerfile/Compose definitions. Display repository, selected ref, detected manifest path(s), last scan, permission status and discovery outcome.
- Discovery lifecycle: `connected -> scanning -> discovered | unsupported | inaccessible -> approved -> imported -> deployed`. Only `approved` projects can enter an execution pipeline. Re-scan must be idempotent and use pagination/rate-limit backoff.
- Repositories with several candidate manifest files require an explicit file/subdirectory choice before deployment. Do not recursively assume every Dockerfile is an independent application.
- Provider webhooks and periodic sync are optional; both must distinguish "new revision available" from "approved to deploy". A rejected or unsupported repository must not create runtime resources.
- Optional webhooks/branch watching are opt-in; retries and duplicate events must not cause duplicate deployments.
- Do not trust arbitrary repositories, Git submodules, remote build contexts or build-time scripts merely because they came from a recognised Git provider.

### FR-03. Prebuilt OCI image (P1)

- Accept named images/tags and preferably immutable digest references; resolve and persist digest used for a release.
- Require explicit runtime configuration (port, protocol, env, health, mounts), with sensible optional defaults; no Dockerfile build required.
- Authenticate to private registries without logging credentials. No OCI registry server is built into Ship.
- Warn when an image tag is mutable; tag drift must not silently change a running release.

### FR-04. Manifest detection and import (P0/P1)

Detection preference: `compose.yaml`, `compose.yml`, `docker-compose.yaml`, `docker-compose.yml`, then root `Dockerfile`. Allow `--file`/`--subdir` for ambiguity and monorepos.

**Dockerfile:** Build one image and generate a minimal Compose service under Ship's environment-specific Compose project. Ask for a container port when unknown; `EXPOSE` is a hint, not proof of a running HTTP listener. Support web, worker and non-HTTP service types.

**Compose:** Validate with `docker compose -f <file> ... config --format json` under a controlled environment; consume canonical service names, builds, images, health checks, volumes, networks and declared dependencies. Treat validation output as sensitive because variable expansion may expose credentials. Keep imported Compose semantics wherever safe. Support multiple services inside one environment.

**Effective Compose model:** Create a deterministic, versioned effective definition combining user Compose plus **explicitly permitted** Ship overrides (labels, environment scope, safe routing/network attachments, managed env/secrets, resource limits). Present the resulting configuration diff before first deployment or sensitive changes. Never silently overwrite user networking, expose a port, change volumes or enable privileges.

**Safety policy:** Reject privileged mode, host PID/network, Docker socket mounts, dangerous capabilities, arbitrary host bind mounts and device passthrough in the initial trust profile. Explain the restriction and keep an audited advanced/trusted override as a later possibility. Do not turn security checks into a false claim of sandbox isolation.

### FR-05. Build, release and deploy pipeline (P0)

Deployment phases: `queued -> fetching/uploading -> validating -> building/pulling -> preparing -> deploying -> verifying -> succeeded | failed | interrupted` (skip inapplicable phases).

- Persist an operation **before** doing work, with a stable idempotency key and typed progress events.
- Limit simultaneous builds to **one by default** and serialize mutations by environment; allow reads/log viewing during builds.
- Execute Docker/Compose using direct structured process arguments, with no shell interpolation or arbitrary user-supplied command flags.
- Label Ship-managed resources with stable project/environment/service/release IDs; honour official Compose project labels for mapping.
- Capture build logs durably, with retention and secret-redaction. Avoid retaining plaintext generated Compose configuration in logs.
- Use `docker compose up -d --wait` where compatible; distinguish `healthy`, `running_unverified`, `starting`, `unhealthy`, `failed`, `stopped` and `unknown`.
- Don't claim readiness just because the Compose subprocess exited successfully; explicit health probes take precedence.
- Preserve named volumes and user data across update, stop, restart and normal removal. Never use `down -v` implicitly.
- After each build, record image ID/digest and effective runtime configuration fingerprint as release metadata where possible.
- On failure, preserve evidence and mark the attempted revision/release failed. Do not corrupt the prior source snapshot or silently erase containers/data.
- No promises of transactional database migration, zero downtime or instant rollbacks on a single host.

### Source-to-release invariants

- A source **revision** is immutable once imported; later local pushes generate different revisions even when they have no Git identity.
- A **release** pins one or more built/pulled images and the resolved non-secret runtime definition. A **deployment** records an attempt to apply that release to a particular environment. A new deployment attempt does not rewrite old release records.
- For Compose, Ship may apply multiple services as one environment deployment operation; success/failure should reflect individual service health as well as the operation-level result.
- Build artifacts are not a trusted security boundary. Host administrators and the Docker daemon remain privileged; a permitted malicious Dockerfile/Compose build can still harm the host.

### FR-06. Release history and rollback (P1, staged)

- Every successful deployment links source revision, release image references, effective non-secret config fingerprint, environment, actor, timestamps and health result.
- **Initial operation:** `redeploy from previous source revision`; label clearly that image layers or base images might change on rebuild. This is **not** an exact rollback.
- **PaaS-grade rollback later:** deploy a retained image ID/digest and previously stored runtime definition, without rebuilding. Verify image availability before allowing rollback and clearly indicate when it has been garbage-collected.
- Retain a configurable small number of releases/snapshots. Never garbage-collect an active release or a persistent volume.
- Database schemas, external dependencies and data are **not** rolled back automatically; present a warning on rollbacks involving stateful services.

## 6. Environments, services, resources and networking

### FR-07. Environments (P1, foundational schema P0)

- Auto-create `development` on project import. Add named environments such as `staging`, `production` and optional ephemeral `pr-123` previews later.
- Each environment owns its service desired state, release assignments, config/secret bindings, routes, Compose namespace, networks and independently scoped volumes.
- Support clone-configuration from an environment **without copying secret plaintext or stateful volume data**. Require deliberate secrets re-binding and resource creation.
- Environments can be started/stopped/redeployed independently. Show environment status aggregation based on service health, not a single misleading Boolean.
- Early implementation may deploy an entire imported Compose environment atomically at the **operation** level, not transactionally at Docker/data level. Per-service changes can come later.
- Preview environments are P2: optional branch/PR naming, expiration/garbage collection, quotas and explicit creation. Never generate unlimited previews from webhooks.

### FR-08. Service types and runtime configuration (P1)

- **Web/API service:** HTTP/TCP listener with explicit port, health checks and optional ingress route.
- **Worker service:** long-lived process without mandatory inbound route; Docker restart policy governs restarts.
- **Job service:** one-shot command with exit code and captured logs; scheduled/cron jobs are P2, using existing trusted task execution rather than a scheduler cluster.
- **Imported Compose service:** preserve declared Compose behaviour within the accepted policy profile.
- Support resource limits (memory/CPU), restart policy, startup timeout, health probes and controlled scaling where Docker Compose semantics permit. **Default replicas = 1; no auto-scaling.**
- Display environment and service configuration with secret values redacted and configuration change history recorded.

### FR-09. Networking, routing and service discovery (P1; host:port P0)

**Two distinct concepts:** (a) a project/service catalogue for humans and the API; (b) runtime connectivity via Docker networking and optional ingress. Do not call the catalogue a custom distributed service-discovery protocol.

- P0: report actual published `host:port` endpoints accurately; detect port conflicts and avoid exposing an arbitrary service unless its definition or user configuration requests it.
- P1: optional Caddy-backed HTTP routing, with a single ingress instance and explicit routing records per environment/service. Configure routes transactionally with validation and rollback on proxy configuration failure.
- Assign friendly hostnames such as `app.dev.example.test` **only when an operator-configured DNS/resolution strategy is available**. Ship must document host DNS, local resolver or manual hosts-file requirements. The `.test` hostname in documentation is illustrative and does not resolve automatically. Provide direct host:port fallback.
- Keep management API/pairing and application ingress on separate listeners/ports. Default management exposure to loopback and opt-in private network access; do not publish applications or management endpoints to the public Internet without explicit operator configuration.
- TLS: require authenticated transport with verified server identity for remote management. Support trusted certificates or an explicitly verified/pinned self-signed key during pairing. Public app domains and ACME are opt-in later. For local application domains, support operator-provided local CA/cert or documented private HTTP; never imply Caddy can issue publicly trusted certificates for arbitrary internal names.
- Compose internal DNS handles intra-project service names. For **cross-project** connectivity, require explicit network/endpoint configuration or resource attachments; do not connect every project to one unrestricted common network by default.
- Avoid unexpected path-prefix routing, since many applications assume they run at `/`. Prefer virtual-host routing when DNS is configured.
- Expose metrics/health and routing configuration in UI, including a clear diagnostic when host DNS does not resolve.

### FR-10. Environment variables and secret management (P1; safe foundations P0)

- Non-secret config: typed project/environment/service-scoped values, with explicitly defined precedence (service overrides environment overrides project defaults). Report source of each effective variable.
- Secrets: authenticated creation/update/delete, encrypted transport, protected file/system storage, restricted access and optional OS keyring/age-based encryption at rest after design review. Avoid inventing a claim that SQLite data is inherently encrypted.
- Never accept secret values as mandatory command-line positional arguments or print them in shell history, stdout, audit logs, API responses or Compose previews. Prefer `--prompt` or stdin.
- Generate runtime env/secret material into permission-restricted temporary files or approved Docker mechanisms; do not bake secrets into images or persist sensitive rendered Compose configuration.
- Rotate secrets by updating bindings and requiring a controlled restart/redeployment when the underlying process cannot reload them.
- Warn that application code, Docker privileges, process environments and host administrators can access secrets: this is trusted-user development, not hardened multi-tenant isolation.
- P0 ingestion must explicitly exclude `.env`/credential files by default; applications needing secrets before the secret UI exists require documented deliberate host-side configuration, never a covert upload bypass.

### Deployment configuration precedence and preview

Ship must calculate an explicit effective configuration for each service. For non-secret application settings, use this order, from lowest to highest priority: source/manifest defaults, project defaults, environment overrides, service overrides, deployment-specific overrides. Some Compose-provided keys may be incompatible with Ship-level overrides; reject ambiguous merges and show a reviewable diff rather than silently changing behaviour. Secret values should be referenced by ID and resolved only immediately before the runtime needs them, without being persisted in preview payloads.

### FR-11. Managed resource templates (P1/P2)

Resource templates should feel like lightweight PaaS provisioning, not a new database product.

- Start with **PostgreSQL and Redis**; optional generic persistent volume template. Templates have explicit image versions, sensible default limits, health checks, private networking and Docker named volumes where appropriate.
- Provision resource instances per environment by default. Record template version, image identity, storage ownership, endpoint, health and attachments.
- `attach` produces a *reviewable* application config/secret binding such as database URL; it never modifies application source and never exposes a database port publicly by default.
- Resource lifecycle is independent from application redeploy. Stopping/redeploying an application must not inadvertently delete the database or its volume.
- Present conspicuous warnings and separate confirmation for irreversible resource deletion. Backups are not automatic just because a named volume exists.
- P2: explicit backup/export/import support, restore validation, maintenance UI and retention controls. No claim of HA or managed database durability.
- Compose-imported database containers remain Compose services until deliberately converted/adopted with a documented migration path; avoid silently changing existing data ownership.

## 7. Runtime reconciliation and reliability

### FR-12. Desired-state reconciler (P0 skeleton, P1 mature)

Ship needs a **small reconciliation loop**, not a distributed scheduler. SQLite is authoritative for owner intent; Docker is authoritative for actual container state.

```text
User action / approved automation
          |
          v
  Desired state in SQLite
          |
          v
 Reconcile (on event + bounded periodic sweep)
          |
      Inspect Docker
          |
          +-- matches desired -> record observation / health
          |
          +-- recoverable drift -> request bounded correction
          |
          +-- unsafe/ambiguous -> flag action required, do not destroy data
```

- Define desired states: `running`, `stopped`, `removed` (with explicit preservation rules). A failed deployment does not automatically mean user intent changed to `stopped`.
- On daemon startup, inspect Docker-labelled objects, reconstruct observations, mark abandoned in-progress operations `interrupted`, and reconcile conservatively.
- **Do not fight the user:** if an externally stopped container is observed, show drift; in automatic-recovery mode, apply the declared recovery policy after a bounded delay. If the environment was intentionally stopped through Ship, never restart it.
- Prefer Docker restart policies for straightforward process restarts. Ship reconciles release/configuration changes, missing resources and drift not covered by Docker's own policies.
- Make recovery policy configurable per environment (`observe_only` default for early releases; `repair_safe_drift` optional later). Require explicit confirmation for adopting/deleting unmanaged resources or overwriting manually modified runtime configuration.
- Cap retries with exponential backoff and circuit-breaker/cooldown to avoid endless rebuild loops or a crashing app consuming the host.
- Use per-environment mutation locks, operation idempotency keys and an in-process queue backed by durable SQLite state. No separate message broker.
- Successful deployment means the requested release was applied and met the configured verification criteria; `running_unverified` is a valid transparent outcome.
- Reconciliation must never auto-drop database volumes, change external host files or rewrite unmanaged Docker objects.

### Reconciliation ownership and failure states

- Ship owns only resources it created or that were **explicitly adopted**; the owner must be able to inspect the Docker labels and ownership mapping.
- The reconciler must serialize against in-flight deployments and must not race with start/stop/removal actions.
- Offline Docker Engine is `unknown/unreachable`, not `stopped` or `healthy`. Unknown must not trigger destructive cleanup.
- `restart` changes observed process lifecycle but must not alter the pinned release; `redeploy` reapplies a known release; `update` creates or selects a new release; `rollback` selects an older retained release. These actions must not be conflated.
- Resource reconciliation is conservative: it never destroys or recreates a persistent database volume as a side effect of application drift detection.

### FR-13. Health, monitoring and operations (P0/P1)

- Read host CPU, memory, disk and Docker availability. Report container CPU, memory, health, restart count and state when supported.
- Stream ordered build/deploy events with SSE; support reconnect using persisted cursors/sequence numbers and a fresh status snapshot.
- Show per-service logs with tail/follow, time range and bounded retention. Redact known secrets but do not promise perfect detection of user-written sensitive content.
- Surface deployment duration, phase-level failures, recent restart loops, low disk space, host reboot/offline conditions and route failures.
- A deployment history timeline connects source revision -> release -> deployment -> observed outcome. Don't confuse desired and observed state.
- Optional alerting, OpenTelemetry exporter and richer metrics are later features, not prerequisites for the first build.

## 8. Frontend product specification

Ship should look and behave like a polished developer platform, not a generic container-administration screen. The compiled Svelte UI is embedded in the Rust daemon and needs no running Node.js server.

### Navigation and views

1. **Overview:** host online/offline, capacity, running/failed/stopped services, active operations, recent deployments and alerts.
2. **Projects:** searchable/filterable project registry with source, environments, status, service count and last deployment.
3. **Project detail / service canvas:** visual map of **declared** services, resource attachments and routes; show health and dependency edges. Provide an accessible list alternative and never invent relationships.
4. **Environment switcher:** select development/staging/etc; independent endpoints, deploys, variables, resources and runtime state.
5. **Service details:** configuration, current release, desired/observed state, health, metrics, ports/routes, restart and scaling controls where supported.
6. **Deployments:** release timeline and phase pipeline: Source -> Build -> Release -> Deploy -> Health; per-phase logs, errors, actor and safe rollback actions.
7. **Logs:** server-rendered/streaming build and application logs with filters, cursors, refresh and copy/download where authorised.
8. **Resources:** template catalogue, provisioned instances, connections, persistent storage and irreversible deletion warnings.
9. **Configuration:** project/environment/service variables, protected secret inputs and route/domain settings.
10. **New project:** import Git URL, select prebuilt image, or show copyable `ship push` onboarding instructions; always validate before running code.
11. **Settings:** target identity, pairing/revocation, ingress/DNS/TLS status, quotas, retention and server version.

### Critical end-user flows

1. **First install:** Visit the server UI on a trusted network, see Docker readiness and host capacity, and complete owner setup/pairing.
2. **First direct push:** The CLI reports source inclusion, transfer, build, deploy, health outcome and real access address. The same operation appears in the UI.
3. **Git import:** View discovered eligible repositories, select a manifest/branch, review requested ports/mounts/privileges, then approve and deploy.
4. **Operate service:** Open project -> environment -> service to inspect logs, effective non-secret settings, resources, ingress, current release, and actions.
5. **Failed release:** See precisely whether upload, validation, build, start, routing or verification failed. Previous healthy release and volumes remain present when possible.
6. **Resource attach:** Provision a database with a private endpoint, attach it to a service and inject the resulting connection reference safely. Never place plaintext credentials into the frontend's general project canvas state.
7. **Rollback:** Select a retained release, review exactly what will change, and explicitly acknowledge that databases/external dependencies will not be rolled back.

### UX requirements

- Responsive desktop-first interface, light/dark mode, accessible labels/keyboard navigation, clear semantic colours, useful empty/loading/error states.
- Never optimistically show a deployment as healthy before the server reports it; distinguish `running_unverified` from `healthy`.
- Provide deterministic refresh and SSE reconnect handling; gracefully present an offline/unreachable host.
- Destructive operations use clear confirmation including project/environment/resource scope and whether data will be preserved.
- The canvas is a **view**, not a separate authoritative orchestration graph. All operations act on the domain model.
- Keep the UI fast with paginated logs and selective polling rather than continuous high-frequency telemetry.

## 9. API and event contracts (proposed v1)

All management endpoints use `/api/v1`, JSON or streaming upload, stable typed errors and appropriate authentication/authorisation. Mutations are operation-driven, returning IDs to poll/stream. Use OpenAPI or generated API types so Rust and TypeScript remain aligned. No GraphQL requirement.

| Method | Path | Purpose | Milestone |
| --- | --- | --- | --- |
| `GET` | `/healthz` | Minimal liveness, no private details | M0 |
| `POST` | `/api/v1/pair` | Redeem short-lived pairing code for CLI token | M1 |
| `GET` | `/api/v1/host` | Host status, Docker readiness, capacity | M0 |
| `GET/POST` | `/api/v1/projects` | List/create projects | M1 |
| `GET` | `/api/v1/projects/{project_id}` | Project details and aggregated state | M1 |
| `GET/POST` | `/api/v1/projects/{project_id}/environments` | List/create environments | M2 |
| `GET` | `/api/v1/environments/{environment_id}` | Environment services/resources/routes | M1/M2 |
| `GET` | `/api/v1/environments/{environment_id}/services` | Service state, health, endpoints | M1 |
| `POST` | `/api/v1/uploads` | Create quota-bound upload session | M1 |
| `PUT` | `/api/v1/uploads/{upload_id}/content` | Stream and verify ZIP | M1 |
| `POST` | `/api/v1/projects/import` | Import upload/Git/image source into project/env | M1/M4 |
| `POST` | `/api/v1/environments/{environment_id}/deployments` | Deploy selected source/release | M1 |
| `GET` | `/api/v1/environments/{environment_id}/deployments` | Deployment/release history | M1 |
| `POST` | `/api/v1/environments/{environment_id}/actions` | Start/stop/restart/redeploy/remove | M1 |
| `GET` | `/api/v1/operations/{operation_id}` | Current durable operation state | M1 |
| `GET` | `/api/v1/operations/{operation_id}/events` | SSE events/progress with cursor | M1 |
| `GET` | `/api/v1/services/{service_id}/logs` | Paginated or streamed runtime logs | M1 |
| `GET/PUT` | `/api/v1/services/{service_id}/config` | Non-secret config and runtime policy | M2 |
| `GET/POST/DELETE` | `/api/v1/environments/{environment_id}/secrets` | Redacted secret metadata and updates | M2 |
| `GET/POST` | `/api/v1/environments/{environment_id}/routes` | Routing records and validation | M2 |
| `GET/POST` | `/api/v1/environments/{environment_id}/resources` | Provision managed dependencies | M3 |
| `POST` | `/api/v1/resources/{resource_id}/attachments` | Attach resource to service | M3 |
| `POST` | `/api/v1/environments/{environment_id}/rollback` | Retained-release restore | M3 |
| `GET` | `/api/v1/events` | Authenticated server event stream | M2 |

**Write endpoints** require idempotency keys, input validation and stable operation semantics. `GET` operations must not mutate runtime state. Remove/secret/resource APIs need strict CSRF/auth handling. Session cookies and CLI tokens are distinct.

### API consistency requirements

- All listed paths are **proposed v1 contracts**, not commitments that they are implemented in M0. Define an OpenAPI schema, typed Rust request/response models and generated or validated TypeScript client types.
- Browser cookie sessions are protected with CSRF controls. CLI bearer tokens are scoped to the trusted host, stored with restrictive permissions and revocable. `healthz` contains no sensitive host metadata.
- An upload session may be resumed only if a resumable protocol is intentionally implemented. Otherwise, interrupted transfer cleans its temporary content safely and permits a new idempotent attempt.
- Long operations return `202 Accepted` with operation URL and ID; clients poll or subscribe to SSE events. Mutating actions require an idempotency key and return a consistent duplicate response for replayed requests.
- Responses for resource deletion or rollback must report data implications explicitly; status fields must never confuse a successful request with a healthy application.

### Operation record example

```json
{
  "operation_id": "op_01...",
  "project_id": "prj_01...",
  "environment_id": "env_01...",
  "phase": "building",
  "status": "running",
  "revision_id": "rev_01...",
  "created_at": "<ISO-8601 timestamp>"
}
```

### SSE event example

```json
{
  "sequence": 42,
  "operation_id": "op_01...",
  "type": "deployment.phase_changed",
  "phase": "verifying",
  "message": "Waiting for web health check",
  "timestamp": "<ISO-8601 timestamp>"
}
```

Error format: `{ "code": "COMPOSE_VALIDATION_FAILED", "message": "...", "details": {}, "operation_id": "..." }`. No credential-bearing process arguments, secret values or sensitive rendered Compose documents in errors or logs.

## 10. Storage, lifecycle and configuration

```text
/var/lib/ship/
  ship.db                        # SQLite WAL + migrations
  projects/<project-id>/
    revisions/<revision-id>/source/    # validated immutable source snapshots
    releases/<release-id>/             # effective non-secret release metadata
    environments/<environment-id>/     # generated runtime definitions / overrides
  uploads/tmp/                   # quota-bound temporary ZIPs
  operation-logs/                # bounded durable event/log stream
  secrets/                       # restricted ownership; encrypted-at-rest mechanism TBD
  ingress/                       # validated Caddy config, only if enabled
  backups/                       # optional explicit metadata export, not automatic volume backup
```

Suggested initial SQLite tables (revise with migrations, not hand-edited schema):

`hosts`, `projects`, `environments`, `services`, `resources`, `resource_attachments`, `sources`, `revisions`, `releases`, `deployments`, `operations`, `operation_events`, `routes`, `config_values`, `secret_metadata`, `runtime_observations`, `auth_tokens`, `web_sessions`, `audit_events`.

- Server-side secrets must not be stored as ordinary plaintext values in `config_values`; document the chosen encryption/permission strategy before implementing the secrets milestone.
- Use atomic SQLite transactions for domain mutations; Docker execution is **not** part of a database transaction.
- Persist operation status before external side effects and reconcile incomplete actions after reboot.
- Retain at least three source revisions by default, with quota-based pruning of unused snapshots and images. Never delete active releases or Docker volumes in ordinary garbage collection.
- Back up SQLite metadata/source snapshots separately from application data. Document recovery from a corrupted disk or failed host; SQLite alone is not an app-data backup.
- Project removal stops/removes managed app containers/networks and unregisters logical records, **preserving volumes** unless a separately confirmed destructive action is selected.

### Installation, upgrades and uninstallation (public distribution contract)

- **Initial support tier:** Linux amd64 server with Docker Engine, Docker Compose v2 and systemd, verified in CI and on a clean installation. Ubuntu LTS may be the first validated distribution but is not a product-wide assumption. Support other Linux distributions and arm64 only when real integration tests and build artifacts exist.
- **CLI support tier:** macOS arm64/amd64 and Linux amd64 initially, with other architectures added after automated and manual verification. A published compatibility matrix is authoritative.
- Publish installable release archives containing `ship-server`, `ship` CLI, checksums, license and concise setup instructions. Provide a systemd unit example and documented user/data paths. Avoid an unreviewed `curl | sudo sh` installation requirement; scripts, if offered, must be auditable and pinned to version/checksum.
- First-run setup creates directories with restrictive permissions, checks Docker/Compose versions, tests daemon access, initialises SQLite schema, starts a loopback-bound API and guides the operator through admin registration/pairing. Do not create a default or universal admin password.
- Split **upgrade** (replace binaries/apply reversible or backed-up migrations) from **uninstall** (remove software). Neither should implicitly delete managed application containers, persistent volumes or user-created data.
- Publish upgrade notes, database migration policy and supported downgrade/restore procedure. Back up the metadata database before destructive migrations and fail safely on incompatible schema versions.
- Document full backup needs: Ship metadata, source snapshots/release metadata, secrets or encryption-key material and application volumes/data. Database metadata backup alone is not a complete recovery of user applications.
- Provide an operator runbook: pairing, certificate management, host shutdown/restart, logs, diagnostics, database backup, Docker access, storage cleanup, data-preserving uninstall and disaster recovery.
- No required online activation, central control service, mandatory GitHub login or hidden phone-home. Diagnostic reporting and update checks, if ever added, must be optional and documented.

### Deployment execution sequence

1. Authenticate source request and resolve project/environment.
2. Stream/capture source or resolve image digest and assign immutable revision.
3. Validate manifest, policy and environmental constraints.
4. Resolve effective configuration, resource attachments, expected ports and route settings; generate preview/diff for sensitive changes.
5. Persist operation, acquire environment mutation lock and enforce build/host quotas.
6. Build/pull image(s), record release identity and non-secret config fingerprint.
7. Apply environment-specific Compose resources, labels, networks and optional Caddy routes.
8. Run configured verification (container/health probes; unknown when none exists).
9. Commit deployment outcome and refresh observed state.
10. Publish CLI/UI result and durable events; schedule conservative reconciliation.

## 11. Security, resource efficiency and operational constraints

### Threat model and controls

- The authenticated operator and deployed source are trusted; source and Compose files can still execute arbitrary build/runtime code. Ship is **not** a secure sandbox for unknown untrusted developers or internet-scale multitenancy.
- Management API binds to localhost until explicitly configured; LAN access uses HTTPS and trusted cert or explicitly verified/pinned fingerprint. Never silently disable verification.
- Pairing codes are short-lived, one-use and generated on the host console; CLI tokens are scoped to an installation and revocable. Browser sessions use secure HttpOnly cookies plus CSRF protection.
- Default management exposure to loopback or explicitly approved private networks and firewall rules. Internet-facing management, if supported, requires explicit hardened configuration and documentation. Never expose the Docker socket remotely.
- Validate paths, archive formats, upload sizes, file count, decompression ratio, extracted total, disk limits and symlink behaviour. Avoid TOCTOU and arbitrary file overwrites.
- Compose policy rejects privileged containers, host namespaces, socket mounts, unsafe device/capability access and unmanaged host filesystem binds in normal mode.
- Docker/build privilege is inherently sensitive. Enforce explicit source approval, runtime limits and auditable policy exceptions; do not claim perfect isolation.
- Secret input via prompt/stdin or protected UI; keep tokens and credentials out of logs, images, build arguments and plaintext API responses.
- Reverse proxy exposure is explicit; a discovered `EXPOSE` instruction does not automatically make an app public.
- Rate-limit authentication and pairing; audit deployment, secret, route, resource and deletion actions.

### Lifecycle on intermittently available hosts

Ship should behave sensibly when a host suspends, changes IP address or spends days powered off. Provide clear host-disconnected UI/CLI errors; do not pretend a build or job continues while the OS is suspended. Detect Docker unavailability and host low disk space before scheduling new work. Hostname/IP changes should be diagnosable, and configured LAN/Tailscale access methods should be documented.

### Portable resource constraints and performance targets

- No hardware model, hostname, user account, storage path or fixed LAN address may be embedded into application logic, tests, defaults or documentation.
- **Development reference host:** Linux amd64 or arm64, a working Docker Engine + Compose v2 plugin, sufficient disk for images/build contexts and RAM appropriate for the workload. The control plane must fail gracefully when resources are insufficient.
- **Performance target (to measure, not assume):** minimise idle memory and CPU usage of the Ship daemon; publish benchmark conditions and results for an agreed low-end and mid-range Linux test host before declaring minimum supported hardware.
- Default to one concurrent build; offer configurable build limits, queue capacity, upload limits, build timeout, cleanup and disk-watermark protection.
- The platform should work without a discrete GPU and must not require one for any core feature.
- Keep the dashboard responsive while builds run and stream progress with bounded memory/backpressure.
- Document differences between rootful Docker access, rootless Docker (only if validated), host firewall policy, systemd-based install and other Linux deployment methods.

## 12. Delivery roadmap and acceptance criteria

The roadmap is deliberately vertical. **Don't implement the entire domain model at once.** Put required identities and relationships into schema early, then add behaviour only when a vertical slice needs it.

### Milestone 0 - Foundations and safe contracts

**Deliver:** Rust workspace and CI, Axum daemon, Svelte frontend shell, CLI skeleton, typed API/domain models, SQLite migrations (project/environment/service/source/revision/deployment/operation), Docker preflight and systemd example, docs and ADRs.

**Acceptance:** Server boots without external DB, UI loads, Docker unavailable shows a useful error, tests/lint pass and unrelated Docker/Podman or other host workloads remain untouched. `development` is understood in the model even if only one environment is enabled.

### Milestone 1 - One-command direct push (first usable PaaS slice)

**Non-negotiable demo:** A sample application is edited on a supported macOS or Linux development workstation (without a Git commit), pushed over an authenticated connection to a freshly provisioned supported Linux host, accessed through a real configured endpoint, inspected in Ship UI, stopped/restarted and redeployed after another edit, with prior deployment history visible.

**Deliver:** Secure target pairing, ZIP pack/upload/extraction, manifest detection, Dockerfile build, generated Compose service, running endpoint/host:port, operation queue + SSE logs, first project/service UI, inspect/start/stop/restart/remove and recovery on daemon reboot.

**Acceptance:** On a macOS or Linux workstation, change an **uncommitted** file in a Dockerfile HTTP app, run `ship push .`, see it hosted on the configured Linux target, access it through an explicitly configured network route/port, inspect its logs/health in UI, and stop/restart it. Invalid ZIP or failed build must not destroy the last good source or app data.

### Milestone 2 - Core micro-PaaS experience

**Deliver:** Compose multi-service import, separate environments, declarative service config/health/limits, environment-scoped networks and volumes, non-secret config and protected secrets, optional managed Caddy ingress with documented local DNS, URL registry, service details, deployment timeline and conservative desired-state reconciliation.

**Acceptance:** Deploy web + API + database Compose services, create a separate `staging` environment, configure a private HTTP route and change a secret without exposing it in source/logs. Stop an app without deleting its database volume. An externally stopped container is shown accurately and auto-repaired only under the declared recovery policy. Ingress is usable only after appropriate local DNS/cert configuration; host:port always remains a documented fallback.

### Milestone 3 - Resources, reliable releases and operations

**Deliver:** PostgreSQL/Redis templates, explicit attachments, resource UI and storage ownership, retained image/release metadata, genuine pinned-image rollback when artifacts are available, stronger observation/metrics, quotas, retention and export/backup documentation.

**Acceptance:** Provision Postgres for one environment, attach it to an app without exposing its port publicly, redeploy app without losing DB data, and restore a previous **retained image release** without rebuilding. UI warns that database contents/schemas are not rolled back. Resource deletion requires separate confirmation.

### Milestone 4 - Git and image deployment

**Deliver:** Public Git repository import, resolved commit SHA, prebuilt OCI image deployment, provider discovery only from explicitly authorised accounts/repositories, optionally read-only GitHub App support for private GitHub repositories and optional provider webhooks.

**Acceptance:** Import a public Git repository containing Compose or a Dockerfile, preview config, explicitly approve, and deploy into the same registry/environment model as direct pushes. Git clone or manifest failure must not execute a workload. Duplicate webhooks do not trigger conflicting deploys.

### Milestone 5 - Optional developer-experience enhancements

**Candidates:** PR previews with TTL and quotas, background jobs and cron, configuration cloning across environments, optional idle suspension, advanced host capacity dashboards, route/TLS enhancements, metadata backup/import, interactive debugging with explicit privilege gates, deployment notification hooks.

**Not a milestone:** Multi-host orchestration, service mesh, HA or multi-tenant billing. Do not introduce them to make Ship feel more enterprise-like.

## 13. Acceptance tests and quality gates

### Essential automated tests

- **Domain:** slug/ID parsing, state transitions, config precedence, environment scoping, resource ownership, release immutability, optimistic concurrency, operation idempotency.
- **Source / security:** `.shipignore` and `.gitignore`, uncommitted file capture, default secret exclusions, ZIP Slip, Unicode/path collision cases, symlinks, oversize/decompression bombs, digest mismatch, invalid TLS/pairing, privilege-restricted Compose.
- **Runtime:** Dockerfile build, Compose stack import, build failure, port conflict, invalid health probe, stable network names, private route, persistent volume preservation, daemon restart, interrupted operation and external drift.
- **Reconciler:** intentional stop remains stopped, safe repair policy respects backoff, reconcile never deletes unmanaged resources, repeated operation does not duplicate a deployment.
- **Environment:** staging/production namespace isolation, independent secrets and volumes, no accidental cross-project networking.
- **Resources:** template provisioning, attachment variable wiring, restart persistence, resource deletion requires deliberate confirmation.
- **Rollback:** snapshot redeploy labelled correctly; pinned-image rollback succeeds without build when image exists and returns actionable failure when missing.
- **API/UI:** browser auth expiry/CSRF, SSE resume after connection drop, accurate status, clear empty/loading/error states, keyboard accessibility, destructive confirmation and no secret leakage.
- **CLI:** macOS arm64/amd64 and Linux amd64/arm64 packaging behaviour (within supported matrix), network interruption/retry, credential redaction, `--json`, detached/waiting operations, nonzero exit on failure.

### CI quality gates

- Rust: `cargo fmt --check`, `cargo clippy -- -D warnings`, `cargo test`, documented toolchain/version pinning.
- Frontend: lint, TypeScript check, unit tests, build, relevant browser UI tests.
- Integration: real disposable Docker environments for CI-capable hosts; do not use mocked Docker success as sole acceptance proof.
- Security: dependency scanning, input fuzz/property tests for archive processing where practical, negative Compose policy tests.
- Documentation: setup, trust assumptions, data preservation, release notes and limitations updated with each milestone.

**Definition of first usable release:** One secure CLI command from a supported development workstation produces a running Dockerfile application on a supported Linux host, shown accurately in the UI with logs, a genuinely accessible configured endpoint and durable deployment history. It needs no cluster, broker or separate database server.

**Definition of micro-PaaS feature-complete release:** Projects, environments and services are first-class; Compose works; apps can have private routes and managed config/secrets; resource templates exist; desired/observed state are reconciled; release history and retained-image rollback work. The system remains single-host and resource-light.

## 14. Instructions for the coding agent

**Agent handoff:** Read the complete spec, inspect existing repository state, write a concise implementation plan and ADR list, then create M0 and the minimum M1 vertical slice. Preserve any pre-existing code or host workloads; never reset repositories or delete unrelated containers or volumes as "cleanup". If the repository is empty, scaffold it as a public-facing open-source project.

1. **Use this v0.4 document as the authoritative product and implementation specification.** It supersedes previous personal-host-specific drafts while preserving direct push, Git import, Compose and PaaS behaviour.
2. Begin with repository scaffold, architecture README, concise ADRs (Docker Compose as runtime, SQLite, single-host, pairing trust model, deployment/release identity, ingress plan, reconciler boundaries), domain types, OpenAPI or generated contracts, and a milestone checklist.
3. Implement Milestone 0, then **the smallest working Milestone 1 vertical slice**: CLI ZIP -> authenticated API -> validated extraction -> Dockerfile build -> Compose launch -> logs/operation -> project/service UI.
4. Define project/environment/service IDs and desired/observed separation early. Do not implement resources, GitHub Apps, Caddy or advanced reconciler features before they are required by their milestones.
5. Use Docker Engine, Compose, BuildKit and optional Caddy as proven foundations. Do not reimplement image building, container scheduling, networking, databases, a general workflow engine or a service mesh.
6. All mutation operations are durable, idempotent where feasible, serialized per environment and observable in CLI/UI. No optimistic false health reporting.
7. **Security is part of the initial vertical slice**, not a later hardening task. Source builds are privileged and only for trusted users/repos; never weaken restrictions silently to pass a demo.
8. Preserve persistent named volumes across ordinary service lifecycle and environment redeploys. Use separate high-friction operations for destructive volume/resource deletion.
9. Deliver meaningful tests alongside each feature. Include real Docker integration smoke tests and a documented verification scenario at each milestone.
10. If requirements conflict, prioritise: security and data safety -> deploy correctness -> simple user experience -> architectural minimalism -> feature breadth. Record unresolved assumptions explicitly.
11. Resist speculative multi-node abstractions and framework-building. Ship should be **a great small PaaS**, not an unfinished cluster orchestrator.
12. Keep installation, development and packaging instructions accurate for each supported OS/architecture combination. Update the support matrix as integration testing expands; do not advertise unverified platforms.

### Suggested very first agent task

> Create the Rust/Svelte workspace and schema foundations for Ship v0.4, including public repository scaffolding, then implement an authenticated `ship push .` vertical slice that deploys a minimal Dockerfile HTTP app to a supported Linux host. Keep environment and service identities in the model, but do not build the full PaaS feature set yet. Demonstrate the end-to-end result, test it against real Docker, and report deferred capabilities and known limitations.

## 15. Open-source distribution, governance and contributor experience

This is a publicly distributed, self-hosted tool, **not a one-person deployment script**. Its documentation, tests and packaging must enable maintainers and contributors who have never seen the original development environment.

### Project repository requirements (P0)

- `README.md`: clear value proposition, architecture at a glance, short demonstration, supported host/client matrix, five-minute quickstart, screenshots, project status and known limitations.
- `LICENSE`: a **specific OSI-approved license chosen by the maintainer before public release**. Do not use the word "open-source" in public distribution without granting the corresponding rights. Apache-2.0 and MIT are candidates; the final choice is a maintainer decision, not assumed by this spec. Add accurate SPDX headers as appropriate.
- `CONTRIBUTING.md`: development prerequisites, local run/test commands, architectural boundaries, PR workflow, issue reproduction and code style.
- `CODE_OF_CONDUCT.md` and `SECURITY.md`: clear contributor expectations, private vulnerability reporting process and supported security versions.
- `CHANGELOG.md`, release/versioning policy, `docs/architecture.md`, ADRs and `docs/operations.md`.
- Issue/PR templates, an example Compose application, a minimal Dockerfile example and separate GitHub/private-provider integration setup documentation.
- Do not publish personal names, real hostnames, home IP addresses, internal URLs, credentials, certificates, private repository examples, screenshots with secrets, or developer-specific defaults.

### Versioning and compatibility

- Use semantic-versioned releases once public APIs are declared stable. Before v1.0, explicitly label which CLI/API/storage interfaces may change, and publish migration notes for breaking changes.
- Separate server version, CLI version and API compatibility. CLI must detect incompatible server API versions and report guidance rather than silently issuing unsupported requests.
- Pin and document supported Docker Engine and Compose **version ranges validated by CI** rather than making unverified blanket compatibility claims.
- Publish a platform matrix for release binaries and installation tests; mark experimental support accurately.
- Version the API (`/api/v1`) and database migrations; maintain OpenAPI documentation or generated equivalent and examples with redacted data.
- Keep UI assets and server binary versions aligned in packaged releases; provide a reproducible asset-build pipeline.

### Build, test and release automation

- CI runs formatting, linting, compiler checks and tests across Rust and frontend, builds release artifacts on a clean environment and verifies checksum integrity.
- Maintain integration tests against an actual disposable Docker host or isolated CI environment; do not confuse Docker-mock tests with proof that a release can deploy containers.
- Add supply-chain checks: dependency audits, secret scanning, release artifact checksums, software bill of materials when practical, and signing/provenance as the release process matures.
- Publish deterministic release notes, supported platform artifacts, checksums, migration notes and a minimal end-to-end demo with each release candidate.
- Include license compliance review for Rust crates, frontend packages and any embedded icons/assets. Do not copy proprietary code or screenshots into the repository without rights.

### Community scope and extension policy

- Accept contributors without requiring membership in a specific cloud ecosystem or proprietary Git provider.
- Maintain a clear public roadmap and distinguish stable functionality from experimental flags and future proposals.
- First-party Git sources should use open protocols where possible. GitHub App integration is optional and scoped; public Git URL import and direct folder push remain vendor-neutral core capabilities.
- Defer user accounts, role hierarchies, plugin marketplace, cluster federation and hosting for unrelated untrusted tenants until explicit use cases justify them.
- Public release must not depend on a maintainer-operated SaaS or telemetry endpoint. Opt-in integrations may be added with transparent configuration and data-flow documentation.
- Before launch, check whether **Ship** conflicts with existing software names or trademarks. Choose repository name, domain, logos and package identifiers only after reviewing availability; do not hard-code an unverified namespace.

### Public documentation acceptance test

A contributor unfamiliar with the original author or machines must be able to: clone the repository, read prerequisites, run tests, build both binaries and UI, install Ship on a clean supported Linux host, pair a supported CLI client, deploy the included sample app, inspect logs, stop/start it, upgrade Ship and uninstall it without unexpectedly deleting data. All examples must use placeholder accounts, standard documentation domains and locally generated secrets.

## 16. Security and support expectations for open-source operators

- Publish a threat model explaining that a trusted user able to build Dockerfiles or deploy Compose files may effectively control the host. Do not market Ship as an isolation boundary for hostile public workloads.
- Use loopback binding by default, authenticated remote access and fail-closed TLS/certificate verification. Initial pairing must require proof of possession and server identity verification; avoid open pairing endpoints by default.
- Never bundle permanent credentials, fixed private keys or default administrative tokens. Credentials must be generated uniquely for every installation and stored with restrictive permissions.
- Ship may execute source supplied by operators. Require explicit approval for Git imports, surface dangerous Compose directives and refuse unsafe configurations according to the documented trust profile; privileged escape hatches must be visible, audited and off by default.
- Provide well-defined maximum upload size, extraction ratio/file-count limits, process timeouts, concurrency limits and host disk-watermark safeguards. Set conservative defaults and make them configurable.
- Document how backups, logs and snapshots can contain sensitive application information; provide secure retention and an explicit purge path that does not accidentally remove unrelated Docker data.
- Have a private vulnerability intake mechanism and a patch/release disclosure policy before requesting community security reports.
- The operator is responsible for reverse proxy/domain exposure, host firewall, OS security updates, trusted source selection and protecting deployed application data. Ship provides safe defaults and actionable warnings but does not replace host administration.

## 17. Primary technical references

- Docker Compose command reference: https://docs.docker.com/reference/cli/docker/compose/
- Docker Compose `up` and `--wait`: https://docs.docker.com/reference/cli/docker/compose/up/
- Compose effective configuration: https://docs.docker.com/reference/cli/docker/compose/config/
- Docker build context and `.dockerignore`: https://docs.docker.com/build/concepts/context/
- Docker restart policies: https://docs.docker.com/engine/containers/start-containers-automatically/
- Docker Compose networking: https://docs.docker.com/compose/how-tos/networking/
- Docker Compose volumes: https://docs.docker.com/reference/compose-file/volumes/
- GitHub repository API: https://docs.github.com/en/rest/repos/contents
- GitHub App authentication: https://docs.github.com/en/apps/creating-github-apps/authenticating-with-a-github-app
- Caddy documentation: https://caddyserver.com/docs/
- Axum: https://github.com/tokio-rs/axum
- Tokio: https://tokio.rs/
- SQLx: https://github.com/launchbadge/sqlx
- Svelte: https://svelte.dev/docs

---

## 18. Revision notes and scope decisions

### v0.4 - open-source productisation

- Removed all assumptions about any named machine, owner, personal repository, specific LAN topology or private workload.
- Defined Ship as a self-hosted, vendor-neutral single-host micro-PaaS with optional GitHub-specific discovery, not a GitHub-only or macOS-only tool.
- Preserved Rust + Svelte + SQLite + Docker/Compose, direct ZIP source push (including uncommitted changes), Git/OCI imports, projects/environments/services/resources, routing, releases, reconciliation and observability.
- Added public installation and upgrades, portable examples, verified OS/architecture compatibility claims, community and contributor docs, release governance, dependency/license checks, open-source security support and distribution requirements.
- Introduced explicit public-release gates for OSI-approved licensing and project-name/trademark availability. Neither is decided by the implementation agent without maintainer input.
- Preserved a phased single-node implementation plan. Do not expand into multi-host orchestration merely to make the product feel enterprise-scale.

**Design invariant:** Ship remains a lightweight, self-hosted, **single-host PaaS**, with a consistent developer experience through CLI and UI. It succeeds when developers can quickly deploy and operate applications on their own infrastructure, not when it reproduces the scheduler and operational complexity of Nomad or Kubernetes.
