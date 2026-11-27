# phase-1-ws-1.7 — Dockerfile deployment

- **Type:** feat | **Risk:** high | **Priority:** p2
- **Coder:** claude-code/opus-5-5 | **Date:** 2026-11-27
- **Plan:** .metis/plans/phase-1-plan.md §1.7

## Goal

Deploy a revision that has a Dockerfile as one durable `deploy` operation
through `validating`, `building`, `preparing`, `deploying` and `verifying`:
generate a single-service Compose project outside the snapshot, publish
`--port` on the daemon's `publish.address` (default `auto`: the host's
tailnet IPv4 address, else `127.0.0.1`) by the `EXPOSE` rule, verify the
service, record the release, the deployment and the route, and on any
failure leave or put the previous release back serving with its data.

## Architectural context

- ADR-0007 rules 1–3, 8; ADR-0008 (the generated file goes through
  `compose config` and the labels override, one deploy path); ADR-0010
  rules 1–3 (route records, conflicts before deploy, `publish.address`);
  ADR-0011 rules 1–3, 7; ADR-0003 rule 4 (no embedded tailnet client);
  ADR-0006 rule 2 (`healthy` only from a passing check); OVERVIEW §3.3,
  §2.3 (all tailnet access in `ship-net`, all Docker in `ship-docker`).
- Plan decisions (Human, 2026-11-27): `publish.address` and the `--port`
  `EXPOSE` rule; a failed deployment runs `up` with the previous release's
  generated file.
- Consumed unchanged: the queue (`http::accept`, `Context::enter`/`log`/
  `build`, `JobFailure`), `upload::snapshot_dir`, the store's revision,
  release, deployment, service and audit repositories, `ship-docker`'s
  `compose::{config, build, up, stop}`, `labels_override`, `containers`,
  `Selector`, `ComposeFiles`, `ComposeProject`, `ServiceStatus::from_observation`.
- New in `ship-core`:
  - `deploy.rs`: `detect(file, exists) -> Detected { kind: Dockerfile |
    Compose, path, note }` (Compose names `compose.yaml`, `compose.yml`,
    `docker-compose.yaml`, `docker-compose.yml` win over `Dockerfile`, and
    the note says so; `--file` must be a relative path inside the snapshot
    and exist; `.yml`/`.yaml` is Compose, anything else a Dockerfile);
    `PortSpec` (`N` or `HOST:CONTAINER`, serialised as that string);
    `ExposedPort`; `map_port(spec, exposed) -> PortMapping` (one `EXPOSE`
    port: host N to it; none: N to N; several: refused naming each, unless
    explicit); `PublishAddress` (`auto` | an IP literal; multicast refused).
  - `api.rs`: `deployments_path`, `DEPLOY_KIND`, `DeployRequest {
    revision_id, file, port }`; `setting_path`, `SETTINGS_KIND`,
    `PUBLISH_ADDRESS_KEY`, `SettingView { key, value, updated_at }`,
    `SetSettingRequest { value }`.
  - `model.rs`: `RouteProvider` (`port`), `RouteStatus` (`pending` |
    `confirmed` | `failed`).
- New in `ship-store`: migration `0003_settings.sql` (`setting` table:
  scope `host` | `project`, key, value, updated_at; 1.8 adds
  `compose.confirm` to it); `host_setting`, `set_host_setting`;
  `replace_routes(env, &[NewRoute])` (one transaction) and `list_routes`;
  `ensure_service` (get-or-create by name); `allocate_release_id` and
  `create_release_as` (the release id is needed before the build, for the
  image tag and the labels).
- New in `ship-docker`: `argv::image_inspect` (`image inspect -- REF`,
  reference checked by `is_image_reference`) and `image::inspect_image ->
  ImageInfo { id, exposed, volumes, labels }`.
- New in `ship-net`: `tailnet.rs` — `TailnetAddress` trait (the host's
  tailnet IPv4 address, or none), `TailscaleCli` (`tailscale ip -4` with a
  bounded wait, 2 s by default; missing, logged out, stopped, slow or
  unparsable is none) and `StaticTailnet` (fixed answer, for tests and for
  a host with no tailnet).
- New in `ship-daemon`:
  - `deploy.rs`: `Deployer` (store, runner, state dir, `DeploySettings`
    { settle, verify_timeout, step_timeout, poll }, build timeout,
    tailnet) and the job. Generated files live in
    `<state>/releases/<release id>/` (`compose.yaml`, JSON, and
    `labels.yaml`, the override), never in the snapshot. Image tag
    `ship_<env id>-<service>:<release id>`. Service `web`, kind `web`.
    `restart: unless-stopped`. Each `VOLUME` of the built image is mounted
    from a named volume of the environment, so its data survives a
    redeploy. Steps: `validating` (detect, deployment row with its
    previous release), `building` (build-only file, `compose config`,
    labels, `compose build` in a build slot, lines to the log),
    `preparing` (inspect the image: id, `EXPOSE`, `VOLUME`; map the port;
    resolve the address; probe it for a conflict unless the environment's
    current route holds it; final file; release recorded with a
    fingerprint of the effective non-secret config), `deploying` (`up
    --remove-orphans`), `verifying` (the new release's container: passing
    check `healthy`; no check, running with no restart after the settle
    period `running_unverified`; anything else `health_check_failed`),
    then the route replaced and the deployment `succeeded`. On a failure
    after `up` ran, `up` with the previous release's files (none: `stop`,
    containers kept as evidence); the deployment is `failed`.
  - `settings.rs`: `GET` and `POST /api/v1/settings/publish.address`; the
    `POST` is a `settings` operation (idempotency key, before any side
    effect) that checks a fixed address is one of the host's (or
    unspecified), stores it and appends an audit event.
  - `POST /api/v1/projects/{project}/deployments` with a `DeployRequest`:
    slug, revision of that project, `file` and `port` checked before any
    operation; then `http::accept` on the project's `development`
    environment.
  - `Config` gains `deploy: DeploySettings` and `tailnet`; `http::router`
    keeps its signature (default deployer), `http::router_with` takes one.
- New in `ship`: `ship settings get|set publish.address [VALUE]`
  (`-o json` on `get`); `set` follows the operation to its end.
- New dependency: `sha2` 0.11 in `ship-daemon` (already locked through
  `ship-source`).

## Declared file scope

- **owned_paths:**
  - crates/ship-core/src/
  - crates/ship-store/
  - crates/ship-docker/
  - crates/ship-net/src/tailnet.rs
  - crates/ship-net/src/lib.rs
  - crates/ship-net/tests/tailnet.rs
  - crates/ship-daemon/
  - crates/ship/src/
  - crates/ship/tests/cli_settings.rs
  - Cargo.lock
  - .metis/briefs/phase-1-ws-1.7.md
- **read_only_paths:** .metis/adr/, .metis/plans/phase-1-plan.md,
  OVERVIEW.md, docs/contracts/cli-json.md, crates/ship/tests/fixtures/,
  crates/ship-source/, .github/workflows/ci.yml

## Definition of Done

- [ ] A Dockerfile project is built, started and reachable on its
      published port against real Docker (`real_deploy.rs`, CI `docker`).
- [ ] Default `publish.address` with no tailnet: the port answers on
      127.0.0.1 and on no other host address; with a faked tailnet
      address (127.0.0.2) it binds that and not 127.0.0.1; with a fixed
      address set it answers there; the address logged as the endpoint is
      the one bound and recorded on the route (real Docker; the generated
      file's `host_ip` is also checked with a fake `docker`).
- [ ] `--port 8080` on an image exposing 3000 serves on host 8080; two
      `EXPOSE` ports fail naming both; `--port 8080:3000` works.
- [ ] A failing build, a port conflict and a failing health check each
      end `failed` with a typed code (`build_failed`, `port_conflict`,
      `health_check_failed`) and the phase, and the previous release
      still serves.
- [ ] The generated Compose file is under `<state>/releases/`, not the
      snapshot, and contains no registered secret value.
- [ ] A named volume survives a redeploy.
- [ ] With no published port, no route is recorded.
- [ ] `publish.address` is stored and served; `ship settings get|set
      publish.address` works; values other than `auto`, a host address or
      an unspecified address are refused; an unset value reads `auto`.
- [ ] `TailscaleCli` treats a missing, failing, slow or garbled
      `tailscale` as no tailnet, within its bounded wait.
- [ ] `metis verify --post` green.

## Test plan

- Unit (`ship-core::deploy`): detection (each Compose name over
  `Dockerfile`, with the note; `--file` kinds, missing, absolute, `..`);
  `PortSpec` parse and serde; `map_port` for 0, 1, 2 exposed ports and the
  explicit form; `PublishAddress` parse.
- Unit (`ship-docker`): `image_inspect` argv and hostile references;
  `ImageInfo` parsing (ports, volumes, nulls).
- `ship-store/tests/deploy.rs`: settings get/set/overwrite; routes
  replace and list; `ensure_service` twice is one row; release with an
  allocated id; migration to version 3 from version 2.
- `ship-net/tests/tailnet.rs`: fake `tailscale` scripts — an address,
  exit 1, a sleep past the wait, garbage, missing program.
- `ship-daemon/tests/deploy.rs` (fake `docker` recording argv): success
  with a route and the generated file's `host_ip`, ports and labels;
  faked tailnet address; fixed address; no `--port` no route; two EXPOSE
  refused at `preparing`; build failure (`build_failed`, `building`,
  nothing started); `up` failing with a port allocation error
  (`port_conflict`, previous release's file brought up again); failing
  health check (`health_check_failed`, `verifying`, previous restored);
  bad requests refused before any operation; settings API over HTTP.
- `ship-daemon/tests/real_deploy.rs` (ignored; CI `docker` job): the
  acceptance criteria above against the real Engine with a busybox
  `httpd` image.
- `ship/tests/cli_settings.rs`: `ship settings get` and `set` against a
  daemon started in-process.

## Out-of-scope touches

None.

## Report

- `ship-core`: `deploy.rs` (new: `detect`, `check_file_path`, `PortSpec`,
  `ExposedPort`, `Protocol`, `map_port`, `AmbiguousPort`,
  `PublishAddress`); `api.rs` (`deployments_path`, `DEPLOY_KIND`,
  `DeployRequest`, `SETTINGS_PATH`, `SETTINGS_KIND`, `PUBLISH_ADDRESS_KEY`,
  `setting_path`, `SettingView`, `SetSettingRequest`); `model.rs`
  (`RouteProvider`, `RouteStatus`). `core::net` is used for `IpAddr`, so
  the purity test's `std::net` ban holds.
- `ship-store`: `migrations/0003_settings.sql`; `settings.rs`
  (`host_setting`, `set_host_setting`); `route.rs` (`NewRoute`, `Route`,
  `replace_routes`, `list_routes`); `repo.rs` `ensure_service`;
  `history.rs` `allocate_release_id`, `create_release_as`
  (`create_release` delegates). `tests/phase1.rs` and `tests/store.rs`
  updated for schema version 3 and the `setting` table; `tests/deploy.rs`
  new (4 tests).
- `ship-docker`: `argv::image_inspect`, `is_image_reference`,
  `MAX_IMAGE_REFERENCE`; `image.rs` (`ImageInfo`, `inspect_image`,
  `parse_image`).
- `ship-net`: `tailnet.rs` (`TailnetAddress`, `StaticTailnet`,
  `TailscaleCli`, `parse_answer`); `tests/tailnet.rs` (3 tests).
- `ship-daemon`: `deploy.rs` and `settings.rs` (new); `http.rs` routes
  `POST /api/v1/projects/{project}/deployments`, `GET|POST
  /api/v1/settings/{key}`, `router_with`; `lib.rs` `Config.deploy`,
  `Config.tailnet`; `Cargo.toml` `serde` moved to dependencies, `sha2`
  0.11 added (`Cargo.lock` updated). `tests/deploy.rs` (13 tests, fake
  `docker`), `tests/real_deploy.rs` (3 tests, ignored),
  `tests/common/mod.rs` (new config fields).
- `ship`: `settings.rs` (`ship settings get|set publish.address`),
  `client::post_json`, `main.rs` dispatch; `tests/cli_settings.rs`.
- `metis verify --post --slice phase-1-ws-1.7`: ALL GREEN (exit 0).
- Not run locally: `real_deploy.rs` (needs a real Engine; this session can
  run only `metis verify`). CI's `docker` job runs it with `--ignored`.
- Decisions:
  - Compose detected without `--file`, or a `.yml`/`.yaml` `--file`,
    fails `invalid_request` in `validating` ("not available yet"):
    Compose deployment is phase-1-ws-1.8.
  - `EXPOSE` and `VOLUME` are read from the built image (`image inspect`),
    not parsed from the Dockerfile, so base-image ports, `ARG`s and
    multi-stage builds count as Docker sees them. The mapping is therefore
    checked in `preparing`, after the build.
  - Each `VOLUME` path is mounted from a named volume named after the path
    (`/var/lib/data` → `var-lib-data`), so data survives a redeploy and a
    later `remove`; networks and volumes keep the environment's first
    release as their release label (Compose would otherwise see them as
    changed).
  - The conflict probe binds the endpoint briefly (TCP or UDP) in
    `preparing`. It is skipped when the environment's current route holds
    that address, for an unspecified address (it would listen on every
    interface) and below 1024 when the daemon may not bind (left to
    Docker); `up` failing with Docker's port-allocation error is
    `port_conflict` too. On macOS a just-released port can read as in use
    for a moment (seen in the unit test, which retries).
  - An unspecified `publish.address` records the route as `0.0.0.0:P`,
    the address bound.
  - Settings changes are `settings` operations (ADR-0011) with an audit
    event (actor `operator`); a fixed address must bind a UDP socket on
    the host, else `invalid_request`.
  - `TailscaleCli` asks the `tailscale` CLI only (not the `tailscaled`
    socket); missing, non-zero, slow (2 s), garbled, loopback or IPv6-only
    answers are no tailnet.
  - A replayed idempotency key on `deployments` or `settings` returns the
    original operation even if the body differs (the queue compares kind
    and environment only, as for every mutation).
  - A deployment interrupted by a daemon stop keeps no result; closing it
    is phase-1-ws-1.14's recovery.
