# phase-1-ws-1.8 — Compose import under the trust profile

- **Type:** feat | **Risk:** high | **Priority:** p2
- **Coder:** claude-code/opus-5-5 | **Date:** 2026-11-27
- **Plan:** .metis/plans/phase-1-plan.md §1.8

## Goal

Deploy a revision whose build definition is a Compose file, with several
services, as one `deploy` operation, after its canonical `docker compose
config` output passed the trust profile in `ship-core` (allow, confirm or
reject, each item naming its key and, where found, its line; ADR-0008 rules
1 to 3), adding only what rules 4 and 5 allow, and asking for confirmation,
or applying the daemon's `compose.confirm` policy, before anything starts.

## Architectural context

- ADR-0008 rules 1–8; ADR-0007 (immutable release, previous release on
  failure); ADR-0011 rules 1–3, 7 (one operation, typed failure with its
  phase); ADR-0006 rule 2 (`healthy` only from a passing check); ADR-0001
  rule 5 (labels); OVERVIEW §3.3 (no secret value persisted, nothing
  started on failure, previous release untouched).
- Plan decisions (Human, 2026-11-27): confirmation without a terminal
  fails (exit 3 is phase-1-ws-1.9's; here the operation fails
  `confirmation_required`); `compose.confirm accept` is held by the daemon,
  never by the pushed files; an imported file keeps the addresses it
  declares (ADR-0008 rule 5), so `publish.address` does not apply to it.
- Consumed unchanged: the queue (`http::accept`, `Context`), `ship-docker`'s
  `compose::{config, build, up, stop}`, `labels_override`, `containers`,
  `inspect_image`, `ComposeFiles`, `ComposeProject`; the store's release,
  deployment, service and route repositories; `ErrorCode::ComposeRejected`
  and `ErrorCode::ConfirmationRequired` (already defined).
- New in `ship-core`:
  - `trust.rs` (pure, no I/O): `evaluate(config: &Value, snapshot_root:
    &str, source: Option<&str>) -> Evaluation`. `Evaluation { items:
    Vec<Item>, snapshot_binds: Vec<SnapshotBind> }`; `Item { outcome:
    Outcome (Confirm | Reject), rule: Rule, service: Option<String>, key:
    String (e.g. `services.web.privileged`), line: Option<u32>, text:
    String }`. Rules:
    - reject: `privileged: true`; `pid`, `ipc`, `network_mode`, `uts`,
      `userns_mode`, `cgroup` set to `host`; non-empty `devices` or
      `device_cgroup_rules`; a bind (service mount, top-level volume bound
      to a host path through `driver_opts`, or a `configs`/`secrets`
      `file`) whose source is a `docker.sock` or a directory holding the
      host's socket (`/`, `/run`, `/var`, `/var/run`);
    - confirm: a bind source outside the snapshot (same three forms); a
      build context or additional context that is a path outside the
      snapshot; any `ports` entry; non-empty `cap_add`; GPU reservations
      (`deploy.resources.reservations.devices`, `gpus`); a `restart`
      other than Ship's default `unless-stopped`, or any
      `deploy.restart_policy`; a mount of an unknown type;
    - allow: everything else (named volumes, networks, health checks,
      `depends_on`, tmpfs, binds inside the snapshot).
    `SnapshotBind { service, target }` lists binds inside the snapshot not
    already read-only. Paths are compared after lexical normalisation.
    Lines come from a best-effort walk of the main file's block YAML (a key
    or list item not found, or a list whose length differs from the
    canonical one, gives the parent's line or none; never a guessed line).
  - `ConfirmPolicy` (`ask` | `accept`); `api.rs`: `COMPOSE_CONFIRM_KEY`,
    `project_setting_path`, and `DeployRequest.confirm: bool` (serde
    default `false`).
- New in `ship-store`: migration `0004_compose_confirm.sql`:
  `audit_event.detail` (nullable JSON text, never a secret value) and
  `setting.updated_by` (default `operator`). `NewAuditEvent.detail`,
  `AuditEvent.detail`, `audit_events_for(action, target)`;
  `project_setting`, `set_project_setting`; `Setting.updated_by`;
  `set_host_setting` takes the actor.
- New in `ship-daemon`:
  - `compose_import.rs`: the Compose path of `deploy::run`, after
    `detect` chose a Compose file. `validating`: `compose config` on the
    file (project directory: the file's directory in the snapshot), the
    profile evaluated; any reject fails `compose_rejected` naming each
    item, before any deployment row; confirm items are listed in the
    events with Ship's diff and the file's fingerprint (`sha256` over the
    file path, its bytes and the item texts) and then: an earlier
    `compose.confirm` audit event of the project with that fingerprint,
    or `confirm: true` (recorded now, actor `operator`), or the effective
    `compose.confirm` policy `accept` (project over host; recorded actor
    `policy`, naming each item, the policy's scope, who set it and when)
    lets it proceed; otherwise `confirmation_required`, nothing recorded
    or started. Then one service row (kind `compose`) per Compose service;
    a release directory with `labels.yaml` (labels, plus `read_only: true`
    for snapshot binds, plus `image: ship_<env>-<svc>:<release>` for a
    built service that names no image) and `files.json` (project
    directory and files: the snapshot's Compose file, then
    `labels.yaml`); `building` (`compose build` when a service builds);
    `preparing` (built images inspected, release recorded, declared host
    ports probed for conflicts unless the environment's routes hold them);
    `deploying` (`up --remove-orphans`); `verifying` every service; routes
    from each published single port (`host_ip`, else `0.0.0.0`). On
    failure the same undo as a Dockerfile release.
  - `deploy.rs`: dispatch to the Compose path; releases now carry
    `files.json` and the undo reads it (falling back to `compose.yaml` +
    `labels.yaml` for 1.7 releases); `verify` checks a list of services.
  - `settings.rs` and `http.rs`: `compose.confirm` (`ask` | `accept`) as
    a host setting (`/api/v1/settings/compose.confirm`) and a project
    setting (`GET|POST /api/v1/projects/{project}/settings/compose.confirm`;
    `GET` is the effective value). Each change is a `settings` operation
    with an audit event.
- Nothing reads a policy from the snapshot, the Compose file (`x-…` keys
  included) or a manifest.

## Declared file scope

- **owned_paths:**
  - crates/ship-core/src/
  - crates/ship-store/
  - crates/ship-docker/
  - crates/ship-daemon/
  - Cargo.lock
  - .metis/briefs/phase-1-ws-1.8.md
- **read_only_paths:** .metis/adr/, .metis/plans/phase-1-plan.md,
  OVERVIEW.md, docs/contracts/cli-json.md, crates/ship/,
  crates/ship-source/, crates/ship-net/, .github/workflows/ci.yml

## Definition of Done

- [ ] Every outcome of rules 1 to 3 has a unit test with a canonical
      Compose document that triggers it, and allow cases trigger nothing.
- [ ] A rejected file fails `compose_rejected` in `validating`, starts
      nothing (no `build`, no `up`), records no deployment, and the
      failure and events name each offending key and line.
- [ ] A file needing confirmation fails `confirmation_required` and starts
      nothing; with `confirm: true` it deploys and the audit log holds the
      confirmation with its fingerprint; the same file redeploys without
      `confirm`; a changed file asks again.
- [ ] A multi-service file with a named volume and a health check deploys
      against real Docker (CI `docker` job), one service row per service.
- [ ] A host-path bind deploys after confirmation against real Docker; a
      GPU reservation deploys after confirmation with the fake `docker`
      (CI has no GPU).
- [ ] With `compose.confirm accept` for a project, its file deploys
      without asking and the audit log names each item, the policy, who
      set it and when; another project still asks; a rejected file is
      still rejected; a policy written in the pushed files is ignored.
- [ ] A snapshot bind is mounted read-only; nothing in `labels.yaml`
      changes a port, network, privilege or a non-snapshot volume.
- [ ] A canary value present only in the interpolated `compose config`
      output is absent from every file under the state directory (database,
      logs, release files) and every API response.
- [ ] `metis verify --post` green.

## Test plan

- Unit (`ship-core::trust`): each reject and confirm rule; allow cases;
  normalisation (`..`, trailing `/`); socket parents; line lookup (block
  maps, sequences, quoted keys, flow style falls back, length mismatch
  falls back); `ConfirmPolicy` parse.
- `ship-store/tests/deploy.rs`: migration to version 4, audit detail and
  `audit_events_for`, project settings beside host settings,
  `updated_by`.
- `ship-daemon/tests/compose_import.rs` (fake `docker` with per-service
  containers): rejected file; confirmation required, then confirmed, then
  silent redeploy, then changed file asks again; `accept` policy per
  project and per host (project wins), audit details; policy in pushed
  files ignored; multi-service deploy with routes and service rows; snapshot
  bind read-only in `labels.yaml`; GPU and host bind after confirmation;
  failing health check brings the previous release's files up again;
  canary grep over the state directory and API answers; settings API.
- `ship-daemon/tests/real_compose_import.rs` (ignored; CI `docker`): the
  multi-service deploy and the host-path bind against the real Engine.

## Out-of-scope touches

None.

## Report

- `ship-core`: `trust.rs` (new: `evaluate`, `Evaluation`, `Item`, `Rule`,
  `Outcome`, `SnapshotBind`, `ConfirmPolicy`, `normalize`,
  `DEFAULT_RESTART`, `SOCKET_PARENTS`, `HOST_NAMESPACE_KEYS`; 12 unit
  tests); `api.rs` `COMPOSE_CONFIRM_KEY`, `project_setting_path`,
  `DeployRequest.confirm`.
- `ship-store`: `migrations/0004_compose_confirm.sql`; `audit.rs`
  (`detail`, `audit_events_for`); `settings.rs` (`SettingScope`,
  `setting`, `set_setting`, `effective_setting`, `Setting.scope`,
  `Setting.updated_by`; `set_host_setting` takes the actor). Tests updated
  for version 4; `project_settings_win_over_the_host` new.
- `ship-daemon`: `compose_import.rs` (new: `check`, `steps`,
  `fingerprint`, `CONFIRM_ACTION`, `ACCEPT_ACTION`, `POLICY_ACTOR`; 4 unit
  tests); `deploy.rs` (Compose dispatch before the deployment row,
  `FILES_FILE`, `release_files`, `write_manifest`, `service_image_tag`,
  `verify` over several services, `up_failure_of`, helpers made
  `pub(crate)`); `settings.rs` (`compose.confirm`, project scope);
  `http.rs` (`GET|POST /api/v1/projects/{project}/settings/{key}`,
  `confirm` passed to the job). `tests/compose_import.rs` (7 tests, fake
  `docker`), `tests/real_compose_import.rs` (2 tests, ignored).
- `ship-docker`: unchanged.
- `metis verify --post --slice phase-1-ws-1.8`: ALL GREEN.
- Not run locally: `real_compose_import.rs` (needs a real Engine; this
  session can run only `metis verify`). CI's `docker` job runs it.
- Decisions:
  - Confirmation items and Ship's diff reach the client as operation
    events (`needs confirmation: …`, `Ship changes: …`, the fingerprint),
    and the `confirmation_required` failure message lists the items; the
    failure shape and the JSON fixtures are unchanged. A client confirms
    by posting the same revision again with `confirm: true`.
  - Only an operator's confirmation (`compose.confirm` audit event) is
    remembered by fingerprint; an acceptance under policy is recorded
    (`compose.accept`, actor `policy`) but does not count as a
    confirmation if the policy later goes back to `ask`.
  - The fingerprint covers the file's path, its bytes and the confirm
    items' texts (which include host paths and line numbers), so the same
    file in a new revision is not asked about again and any change to it,
    or to what interpolation makes of it, asks again.
  - Profile additions within ADR-0008's categories: `uts`, `userns_mode`
    and `cgroup` set to `host` and `device_cgroup_rules` are rejects; a
    top-level volume bound to a host path through `driver_opts`, a
    `configs`/`secrets` `file` outside the snapshot, a build context
    outside the snapshot and an unknown mount type need confirmation;
    `/`, `/run`, `/var`, `/var/run` count as the Docker socket. Other keys
    (`security_opt`, `sysctls`, …) are allowed, as before.
  - Every bind inside the snapshot is mounted read-only (a write would
    change an immutable revision); the override repeats the mount with
    `read_only: true`, which Compose merges by target.
  - A built service the file names no image for is tagged
    `ship_<env>-<service>:<release>`, so the previous release's image
    survives a failed deployment; a built service with its own `image`
    keeps it. Image-only services are recorded in the release by the
    reference the file names.
  - `--port` with a Compose file fails `invalid_request` in `validating`:
    an imported file keeps the ports it declares. Routes are recorded for
    single published ports at their declared `host_ip` (none:
    `0.0.0.0`); ranges and ephemeral ports get none, and the log says so.
  - `docker compose config` failing is `invalid_request` without its
    stderr (it may quote interpolated values; ADR-0008 rule 7).
  - Every service must verify (`healthy` or `running_unverified`); a
    one-shot service that exits fails verification in Phase 1.
  - `GET /api/v1/projects/{project}/settings/compose.confirm` answers the
    value in effect (project, else host, else `ask`); `SettingView` is
    unchanged. The CLI for `compose.confirm` is phase-1-ws-1.9's.
