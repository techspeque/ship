# phase-0-ws-0.1 — Workspace scaffold and verify gate

- **Type:** feat | **Risk:** medium | **Priority:** p2
- **Coder:** claude-code/opus-5-5 | **Date:** 2026-11-25
- **Plan:** .metis/plans/phase-0-plan.md §0.1

## Goal

A Cargo workspace with the seven crates of OVERVIEW §2.3, a pinned toolchain,
a `make verify` gate (fmt, clippy, test), a test enforcing the §2.3
dependency rule, CI running the gate, and the ADR-0012 rule 1 open-source
files.

## Architectural context

- OVERVIEW §2.3: crates `ship`, `ship-core`, `ship-daemon`, `ship-docker`,
  `ship-source`, `ship-store`, `ship-net`. Leaves (`ship-docker`,
  `ship-source`, `ship-store`, `ship-net`) depend only on `ship-core`, never
  on each other; `ship-core` depends on no workspace crate.
- ADR-0002: one Rust binary `ship`; ADR-0002 rule 4 makes the dependency rule
  binding.
- ADR-0012 rules 1, 4, 5, 7, 9: LICENSE MIT, honest README status, SECURITY
  private route, CONTRIBUTING with build/test commands, CI; no real
  hostnames, addresses or names; only CI-tested platforms claimed; pre-1.0
  instability stated.
- `.metis/project.yaml` `commands.verify` is `make verify`.
- No external crates in this slice: placeholders and `ship --version` need
  none, so `Cargo.lock` lists only workspace crates. Clap arrives with the
  first real subcommand (ws-0.4/0.6).

## Declared file scope

- **owned_paths:**
  - Cargo.toml
  - Cargo.lock
  - rust-toolchain.toml
  - Makefile
  - crates/
  - .github/workflows/ci.yml
  - LICENSE
  - README.md
  - SECURITY.md
  - CONTRIBUTING.md
  - .gitignore
  - .metis/briefs/phase-0-ws-0.1.md
- **read_only_paths:** OVERVIEW.md, .metis/adr/, .metis/plans/phase-0-plan.md, .metis/project.yaml

## Definition of Done

- [ ] `Cargo.toml` workspace lists the seven crates under `crates/`; each compiles with a placeholder.
- [ ] `rust-toolchain.toml` pins an exact Rust version with rustfmt and clippy; `Cargo.lock` committed.
- [ ] `make verify` runs `cargo fmt --check`, `cargo clippy --workspace --all-targets -- -D warnings`, `cargo test --workspace`, and passes on a clean checkout.
- [ ] `ship --version` prints the crate version, the git commit (or `unknown` outside a git checkout) and the platform (`os-arch`).
- [ ] A test fails when any leaf crate's manifest names another leaf crate in any dependency section (normal, dev, build, target-specific; key, subtable or `package =` rename), or when `ship-core` names any workspace crate.
- [ ] `.github/workflows/ci.yml` runs `make verify` on `ubuntu-latest` (linux amd64) with read-only permissions.
- [ ] LICENSE (MIT, holder "The Ship contributors"), README (pre-alpha, pre-1.0 instability, CI platforms only), SECURITY.md (GitHub private vulnerability reporting), CONTRIBUTING.md (build and test commands).
- [ ] `.gitignore` covers `target/` and `web/` build output (`web/dist/`, `web/node_modules/`).
- [ ] No real hostname, address or personal name anywhere in the diff.

## Test plan

- `crates/ship/tests/dependency_rule.rs`:
  - `workspace_respects_dependency_rule` — reads every `crates/*/Cargo.toml`, classifies `ship` and `ship-daemon` as top-level, `ship-core` as core, every other crate as a leaf (so a new crate is a leaf by default), and fails listing each violation.
  - `workspace_has_expected_crates` — the seven §2.3 crates exist.
  - `detects_leaf_to_leaf_*` — the checker reports a violation for a synthetic leaf manifest declaring another leaf as a normal dependency, a dotted `.workspace = true` key, a `[dependencies.<leaf>]` subtable, a `package = "<leaf>"` rename, a dev-dependency, a build-dependency and a target-specific dependency; accepts a leaf depending on `ship-core`; rejects `ship-core` depending on any workspace crate; ignores names in comments.
- `crates/ship/src/main.rs` unit test: version line contains version, commit and `os-arch`.
- Gate: `metis verify --post` (runs `make verify`).

## Out-of-scope touches

None.

## Report

- Added: `Cargo.toml` (workspace, edition 2024, version 0.0.1), `Cargo.lock`
  (workspace crates only, no external crates), `rust-toolchain.toml`
  (1.91.0, rustfmt + clippy), `Makefile`, `crates/` (7 crates),
  `.github/workflows/ci.yml`, `LICENSE`, `SECURITY.md`, `CONTRIBUTING.md`.
  Changed: `README.md`, `.gitignore`.
- `ship --version` / `-V` prints `ship 0.0.1 (commit <12-char sha>, <os>-<arch>)`.
  Commit comes from `SHIP_COMMIT` env, else `git rev-parse` in
  `crates/ship/build.rs`, else `unknown`.
- Decisions: `make verify` uses `cargo fmt --all --check` (`--all` covers
  every workspace member regardless of cwd). Added
  `crates/ship/tests/cli_version.rs`, which runs the built binary, beyond the
  unit test in the test plan.
- `metis verify --post`: ALL GREEN. fmt, clippy -D warnings and 16 tests
  (1 unit, 2 cli_version, 13 dependency_rule).
- Not verifiable locally: "CI is green on the integration branch". That needs
  a push to `dev`.
