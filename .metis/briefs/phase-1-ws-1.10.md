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

## Reviewer checklist — cycle 1

- Behavioral correctness: BLOCK f-045 — partial preparation leaves untracked env files that pruning cannot find (`crates/ship-daemon/src/config.rs:668`, `crates/ship-daemon/src/deploy.rs:582-601`).
- Security and authorization correctness: BLOCK f-043/f-044 — rotated secrets lose redaction after restart (`crates/ship-daemon/src/secrets.rs:129`); Clap echoes leading-hyphen secret arguments (`crates/ship/src/vars.rs:118`, `crates/ship/src/main.rs:191-194`); routes retain the origin guard (`crates/ship-daemon/src/http.rs:141-158`).
- Scope discipline: PASS — `metis log phase-1-ws-1.10 --validate -o json` reports `ok: true`, with no out-of-scope files; committed scope is declared at `.metis/briefs/phase-1-ws-1.10.md:96-113`.
- Test sufficiency: BLOCK for the demonstrated regressions — restart coverage does not rotate/unset (`crates/ship-daemon/tests/config.rs:647-652`), CLI cases omit leading-hyphen values (`crates/ship/tests/cli_config.rs:139-144`), pruning covers successful deployments only (`crates/ship-daemon/tests/config.rs:639-645`).
- Architectural fit: PASS — pure precedence rules (`crates/ship-core/src/config.rs:118`), transactional metadata (`crates/ship-store/src/config.rs:266`), and post-build injection (`crates/ship-daemon/src/compose_import.rs:607-629`) fit the declared boundaries and mechanisms.
- Maintainability: PASS apart from the recorded defects — shared submission/following (`crates/ship/src/settings.rs:293-324`) and config preparation (`crates/ship-daemon/src/config.rs:510`) avoid duplicate implementations.

Independent `metis verify --post --slice phase-1-ws-1.10 --force`: ALL
GREEN. Real-Engine tests were not run in this review. Targeted reproductions
used the built CLI and an external temporary harness linked to the built
libraries; no implementation or test files were changed. Verdict: blocked,
findings f-043, f-044 and f-045.
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

- [x] `ship secrets set NAME=VALUE`, or with a value as an extra
      argument, exits 2 before connecting and its output does not contain
      the value; `--stdin` and `--prompt` are the only ways in.
- [x] No API response, operation event, operation log, generated Compose
      file, override, `files.json`, `config.json`, fake `docker` argv
      (build arguments included) or daemon stderr contains a test secret,
      checked by one test that greps them all; with real Docker, the
      image's history and config do not contain it either.
- [x] A deployed service sees config and secret values with the
      precedence project < environment < service (env file with a fake
      `docker`; the container's environment with real Docker, including
      values with `$`, quotes, `#`, spaces and backslashes).
- [x] Changing a value changes nothing running; the lists' `stale_services`
      names the service until a redeploy, then is empty.
- [x] Secret files are 0600 and their directory 0700; env files 0600 in a
      0700 directory; old releases' env files are pruned.
- [x] Secret values stored before a daemon restart are redacted after it.
- [x] Names and values outside the rules, an unknown environment or
      service, and a var and secret sharing a name in one scope are
      refused, the first three before any operation.
- [x] `docs/secrets.md` states that Phase 1 secrets are protected by file
      permissions, not encryption, and who can read them.
- [x] `metis verify --post` green.

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

## Reviewer checklist — re-review

Narrowed to f-043, f-044 and f-045.

- Behavioral correctness: PASS — attempted releases are tracked before preparation and pruned after failure (`crates/ship-daemon/src/deploy.rs:393`, `crates/ship-daemon/src/compose_import.rs:499`).
- Security and authorization correctness: PASS — startup registers retained secret values (`crates/ship-daemon/src/secrets.rs:148`); secret-command parse errors use argument-free diagnostics (`crates/ship/src/main.rs:197`).
- Scope discipline: PASS — `metis log phase-1-ws-1.10 --validate -o json` reports no scope or commit violations; scope remains declared in this brief.
- Test sufficiency: PASS — rotation and unset before restart (`crates/ship-daemon/tests/config.rs:533`), leading-hyphen arguments (`crates/ship/tests/cli_config.rs:152`), and partial preparation failure (`crates/ship-daemon/tests/config.rs:563`) have regression coverage.
- Architectural fit: PASS — retained values reuse release metadata and env files (`crates/ship-daemon/src/secrets.rs:169`); attempted-release cleanup uses the existing deployment outcome path (`crates/ship-daemon/src/deploy.rs:393`).
- Maintainability: PASS — shared env-file decoding and pruning keep the fixes localized (`crates/ship-daemon/src/secrets.rs:201`, `crates/ship-daemon/src/config.rs:673`).

Independent `metis verify --post --slice phase-1-ws-1.10 --force`: ALL
GREEN. Resolved f-043 at bece0b1, f-044 at 4e885c4, f-045 at 53c0d6a
with regression-test evidence. No new findings. Real-Engine ignored
tests were not run locally. Verdict: pass.

## Report

- `ship-core`: `config.rs` (new: `ConfigScope`, `check_name`,
  `check_value`, `check_secret_value`, `MIN_SECRET_LEN`, `resolve`,
  `env_file_line`, `ConfigError`); `api.rs` (`CONFIG_KIND`, the four
  paths, `config_query`, `SetVarRequest`, `SetSecretRequest` with a
  redacting `Debug`, `UnsetRequest`, `VarView`, `SecretView`, `VarList`,
  `SecretList`); `lib.rs`.
- `ship-store`: `config.rs` (new: `ConfigTarget`, `ConfigValue`,
  `SecretRecord`, `new_secret_ref`, set/unset/list for both, `all_secrets`;
  static SQL on `coalesce(project_id, environment_id, service_id)`, as the
  unique indexes); `lib.rs`. No migration: migration 0002's tables serve.
  `tests/config.rs` (4 tests).
- `ship-daemon`: `secrets.rs` and `config.rs` (new); `http.rs` (the four
  routes); `deploy.rs` (`prepare` in `preparing`, `config.yaml` last in the
  files and `files.json`, the fingerprint covers the config, `prune` after
  the outcome); `compose_import.rs` (same, plus a log line naming names the
  file's `environment:` also sets); `queue.rs` (`Queue::redactor`);
  `lib.rs` (`register_all` at start). `tests/config.rs` (3 tests, fake
  `docker`; the grep test runs the scenario in a child process with the
  real stderr logger and greps its stderr), `tests/real_config.rs` (2
  tests, ignored).
- `ship`: `vars.rs` (new: `ship vars`, `ship secrets`); `main.rs`;
  `settings.rs` (`submit`, the submit-and-follow loop, shared with
  `vars.rs`; `new_key` takes the kind). `tests/cli_config.rs` (2 tests).
- `docs/secrets.md` (new).
- `metis verify --post --slice phase-1-ws-1.10`: ALL GREEN (exit 0). One
  earlier run failed in `push::tests::only_an_answering_address_answers`
  (unchanged here: it frees an ephemeral port and asserts nothing answers
  on it, which another test can take meanwhile); the rerun passed.
- Not run locally: `real_config.rs` (needs a real Engine); CI's `docker`
  job runs `ship-daemon` with `--ignored`.
- Decisions:
  - `ship status` (and its `stale_config`) is phase-1-ws-1.11. This slice
    computes staleness (`config::stale_services`, from each release's
    `config.json` stamps) and shows it in the lists (`stale_services`) and
    after every `ship vars|secrets set|unset`; 1.11 reads the same
    function for `stale_config`.
  - Manifest defaults: Ship reads no manifest of its own in Phase 1, so
    the lowest layer is empty; `resolve` takes it as a layer.
  - Compose gives a service's own `environment:` precedence over
    `env_file`; such a name keeps the file's value (ADR-0008 rule 5: Ship
    changes nothing the file declares) and the deployment logs the names.
  - Values are delivered as a double-quoted env file with `\`, `"` and
    `$` escaped. One line only, and no trailing backslash (the env-file
    parser reads `\"` at the end as an escaped quote); both refused with a
    message that quotes nothing. Secrets are at least 4 bytes: the
    redactor replaces every occurrence, so a shorter value would mangle
    the logs.
  - `--service S` added to `ship vars|secrets` (OVERVIEW §6 lists
    `--project` and `--env`; ADR-0009 rule 1 has a service scope). A
    service is known from its first deployment; before that it is `404`.
  - A change is a `config` operation on the project's `development`
    environment, so it never interleaves with a deployment reading the
    config.
  - Unsetting a name that is not set succeeds and says nothing changed.
  - A var and a secret of one name in one scope: `conflict`; across
    scopes the higher scope wins whatever the kind.
  - Env files are kept for the serving release and, after a success, the
    previous one (which a later failure brings back); older ones are
    removed after each deployment. A failed release's env files are
    removed after its failure.
  - A secret is registered with the redactor when its request passes the
    checks, before the operation is submitted, and every stored one at
    daemon start.
