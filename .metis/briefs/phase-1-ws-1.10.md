# phase-1-ws-1.10 — Config values and write-only secrets

- **Type:** feat | **Risk:** high | **Priority:** p2
- **Coder:** claude-code/opus-5-5 | **Date:** 2026-11-27
- **Plan:** .metis/plans/phase-1-plan.md §1.10

## Goal

Let the operator set non-secret config values and write-only secrets for a
project, its environment or one of its services (`ship vars` and
`ship secrets`), store secret values only in 0600 files under the state
directory, inject the resolved values at deploy through a permission-
restricted env file outside the snapshot, never into an image, a build
argument, a saved Compose file, a log, an event or an API response, and
show which services run with values older than the stored ones
(ADR-0009).

## Architectural context

- ADR-0009 rules 1–3, 5–7, 9; ADR-0011 rule 1 (a change is an operation
  before any side effect); ADR-0008 rules 4, 5, 7 (Ship adds managed
  config through its own override; changes nothing the file declares;
  `compose config` output is sensitive); ADR-0007 rule 3 (the release's
  fingerprint covers the effective non-secret config); OVERVIEW §3.3
  (no secret value in any output), §2.3 (dependency rule).
- Consumed unchanged: the store's `config_value` and `secret` tables
  (migration 0002: the secret table has no value column), the queue
  (`http::accept`, `Context`, `JobFailure`), `Redactor`, `SecretValue`,
  the deploy path (`deploy::run`, `compose_import::steps`,
  `release_dir`, `write_file`, `write_manifest`, `ComposeFiles`).
- New in `ship-core`:
  - `config.rs` (pure): `ConfigScope` (`project` | `environment` |
    `service`); `check_name` (an environment variable name:
    `[A-Za-z_][A-Za-z0-9_]*`, 1–256 bytes); `check_value` (at most 64 KiB;
    no newline, carriage return or NUL; not ending in `\`, which Compose's
    env-file parser cannot read back); `resolve` (layers lowest first:
    manifest defaults, project, environment, service; a later layer's
    entry replaces an earlier one's, a var and a secret alike);
    `env_file_line` (`KEY="…"` with `\`, `"` and `$` escaped, so Compose
    reads it back literally and interpolates nothing).
  - `api.rs`: `CONFIG_KIND` (`config`); `vars_path`, `vars_unset_path`,
    `secrets_path`, `secrets_unset_path`, `config_query`;
    `SetVarRequest { env, service, name, value }`, `SetSecretRequest`
    (same fields; `Debug` redacts `value`), `UnsetRequest { env, service,
    name }`; `VarView { name, value, updated_at }`, `SecretView { name,
    updated_at }`, `VarList` and `SecretList` (`scope`, `environment`,
    `service`, the entries, `stale_services`).
- New in `ship-store`: `config.rs` — `ConfigTarget` (`Project` |
  `Environment` | `Service` id); `set_config_value`, `unset_config_value`,
  `list_config_values`; `set_secret` (returns the replaced backend ref),
  `unset_secret`, `list_secrets`, `all_secrets`. A var and a secret of one
  name in one scope are refused (`Conflict`), in the same transaction.
- New in `ship-daemon`:
  - `secrets.rs`: the Phase 1 secret store, `<state>/secrets/` (0700,
    repaired to 0700 at start), one file per value named by a random
    backend ref (0600, created exclusively, fsynced, then the row points
    at it; the replaced file is removed after); `read`, `remove`;
    `register_all` registers every stored value with the `Redactor` at
    daemon start.
  - `config.rs`: the API — `GET|POST /api/v1/projects/{project}/vars`,
    `POST …/vars/unset`, `GET|POST …/secrets`, `POST …/secrets/unset`.
    Project, environment (`development` only in Phase 1), service, name
    and value are checked before any operation; a `config` operation on
    the project's environment then applies the change and appends an audit
    event (names and scope, never a value). A secret value is registered
    with the `Redactor` as the request is read, and a malformed secret
    body is refused with a fixed message that quotes nothing. Lists give
    names, values (vars only) and timestamps, never a secret value.
    For deployments: `prepare` resolves each service's entries, writes
    `<release>/env/<service>.env` (dir 0700, files 0600) and
    `<release>/config.yaml` (an override naming only those env files, so
    every file Compose is given before it holds no value), and
    `<release>/config.json` (a stamp per service: a SHA-256 of the var
    names and values and the secret names and backend refs; no value);
    `stale_services` compares the serving release's stamps with the stored
    config now; `prune` removes the env files of the environment's
    releases other than the one serving and the one before it.
  - `deploy.rs` / `compose_import.rs`: in `preparing`, `config.yaml` is
    added last to the release's files (and `files.json`), so the build,
    the labels override and the trust profile never read a value; the
    release fingerprint includes the services' non-secret config (var
    names and values, secret names). After the outcome, `prune`.
  - `queue.rs`: `Queue::redactor()`.
  - `lib.rs`: `register_all` at start, before anything is served.
- New in `ship`: `vars.rs` — `ship vars set NAME=VALUE`, `ship vars unset
  NAME`, `ship vars list`, `ship secrets set NAME (--prompt | --stdin)`,
  `ship secrets unset NAME`, `ship secrets list`, each with `--project P`
  and optional `--env E` and `--service S`; `list` takes `-o json`.
  `secrets set` refuses `NAME=VALUE` and any extra argument before
  connecting and without echoing it; `--prompt` reads from `/dev/tty` with
  echo off; `--stdin` reads all of stdin, dropping one final newline.
- New doc: `docs/secrets.md` (protection by file permissions, not
  encryption; a service reads its own secrets; Docker access reads all;
  where values are written; value limits).

## Declared file scope

- **owned_paths:**
  - crates/ship-core/src/
  - crates/ship-store/src/
  - crates/ship-store/tests/config.rs
  - crates/ship-daemon/src/
  - crates/ship-daemon/tests/config.rs
  - crates/ship-daemon/tests/real_config.rs
  - crates/ship/src/
  - crates/ship/tests/cli_config.rs
  - docs/secrets.md
  - Cargo.lock
  - .metis/briefs/phase-1-ws-1.10.md
- **read_only_paths:** .metis/adr/, .metis/plans/phase-1-plan.md,
  OVERVIEW.md, docs/contracts/cli-json.md, crates/ship/tests/fixtures/,
  crates/ship-store/migrations/, crates/ship-docker/, crates/ship-net/,
  crates/ship-daemon/tests/, .github/workflows/ci.yml

## Definition of Done

- [ ] `ship secrets set NAME=VALUE`, or with a value as an extra
      argument, exits 2 before connecting and its output does not contain
      the value; `--stdin` and `--prompt` are the only ways in.
- [ ] No API response, operation event, operation log, generated Compose
      file, override, `files.json`, `config.json`, fake `docker` argv
      (build arguments included) or daemon stderr contains a test secret,
      checked by one test that greps them all; with real Docker, the
      image's history and config do not contain it either.
- [ ] A deployed service sees config and secret values with the
      precedence project < environment < service (env file with a fake
      `docker`; the container's environment with real Docker, including
      values with `$`, quotes, `#`, spaces and backslashes).
- [ ] Changing a value changes nothing running; the lists' `stale_services`
      names the service until a redeploy, then is empty.
- [ ] Secret files are 0600 and their directory 0700; env files 0600 in a
      0700 directory; old releases' env files are pruned.
- [ ] Secret values stored before a daemon restart are redacted after it.
- [ ] Names and values outside the rules, an unknown environment or
      service, and a var and secret sharing a name in one scope are
      refused, the first three before any operation.
- [ ] `docs/secrets.md` states that Phase 1 secrets are protected by file
      permissions, not encryption, and who can read them.
- [ ] `metis verify --post` green.

## Test plan

- Unit (`ship-core::config`): names, values, `resolve` order across all
  four layers, `env_file_line` escaping; `SetSecretRequest`'s `Debug`.
- `ship-store/tests/config.rs`: set, overwrite, unset and list per scope;
  the var/secret name conflict; `set_secret` returns the replaced ref;
  `all_secrets`.
- `ship-daemon` unit: secret store modes, replace and remove; stamps.
- `ship-daemon/tests/config.rs` (fake `docker`): the API for vars and
  secrets (set, list, unset, refusals before any operation, conflict as a
  failed operation); a deploy with values at three scopes (env file
  content, modes, `config.yaml` last in `up`'s files and holding no
  value); stale after a change, not after a redeploy; the secret grep over
  responses, events, logs, release files, the fake's argv and the
  daemon's captured stderr; redaction after a restart; pruning.
- `ship-daemon/tests/real_config.rs` (ignored; CI `docker` job): a busybox
  image prints its environment; precedence and literal values; `docker
  history` and `image inspect` hold no secret.
- `ship/tests/cli_config.rs`: the refusals of `ship secrets set` before
  connecting; `vars set|list|unset` and `secrets set --stdin|list|unset`
  against an in-process daemon; `-o json` lists.

## Out-of-scope touches

None.

## Report

Filled at implementation.
