# phase-0-ws-0.9 — Release pipeline and install script

- **Type:** feat | **Risk:** medium | **Priority:** p2
- **Coder:** claude-code/opus-5-5 | **Date:** 2026-11-25
- **Plan:** .metis/plans/phase-0-plan.md §0.9

## Goal

A `v*` tag builds four release archives with the UI embedded, linux amd64
and arm64 static binaries and macOS amd64 and arm64. It publishes them with
a checksum file, signatures and an SBOM. A user installs with `install.sh`,
which refuses anything whose signature or checksum does not verify. CI
audits Rust and web dependencies for known vulnerabilities and licences.

## Architectural context

- ADR-0012 rule 2 (checksums, signatures, SBOM and a verifying `install.sh`
  from the first tag; no `curl | sudo sh` unverified), rule 3 (no
  phone-home), rule 4 (no real hostnames or names; the GitHub repository
  path is the release origin and is already public in the remote), rule 5
  (README lists exactly what CI builds and tests).
- ADR-0002 / OVERVIEW §3.1: one static binary for linux amd64/arm64 and
  macOS amd64/arm64; `ship daemon` is Linux only; no Node.js at runtime.
- Consumes, unchanged: `crates/ship/build.rs` (`SHIP_COMMIT` env for release
  builds), `crates/ship-daemon/build.rs` (`SHIP_REQUIRE_UI=1` makes a
  missing `web/dist` a build error), `ship --version` line format, `ship
  target add NAME local --socket PATH`, `ship projects -o json`, `ship
  daemon --state --socket --port`, `ship dial-stdio --socket`.
- Metis's own `install.sh` was not readable from this session. The script
  follows the same shape (detect platform, download to a temp dir, verify,
  install), as rewritten here.

Design decisions:
- **Targets:** `x86_64-unknown-linux-musl` on `ubuntu-latest` and
  `aarch64-unknown-linux-musl` on `ubuntu-24.04-arm`, each built natively
  with `musl-tools`, so the bundled SQLite C code is compiled by `musl-gcc`
  and no cross toolchain is needed. Both macOS targets build on
  `macos-latest`. `cargo build --release --locked`, with
  `SHIP_REQUIRE_UI=1` and `SHIP_COMMIT=<sha>`. The UI is built once in its
  own job and handed to every build job as an artifact.
- **Archives:** `ship-<tag>-<os>-<arch>.tar.gz` with `os` in
  {linux, darwin} and `arch` in {amd64, arm64}. Each holds `ship`, `LICENSE`
  and `README.md` in a top-level `ship-<tag>-<os>-<arch>/` directory.
- **Release files:** the four archives, `ship-<tag>.spdx.json` (SBOM from
  Syft over the source tree: `Cargo.lock` and `web/package-lock.json`),
  `SHA256SUMS` (the archives and the SBOM), one `<file>.sigstore.json`
  bundle per archive, for the SBOM and for `SHA256SUMS`, and `install.sh`.
- **Signatures: Sigstore cosign, keyless**, signed by the release workflow's
  GitHub OIDC identity. There is no signing key to create, store or leak.
  The verifier pins identity
  `https://github.com/techspeque/ship/.github/workflows/release.yml@refs/tags/<tag>`
  and issuer `https://token.actions.githubusercontent.com`. Cost: `cosign`
  must be installed on the machine running `install.sh`. Without it the
  script refuses and names how to get cosign. There is no skip flag.
- **Version guard:** the release fails unless the tag equals `v` + the
  workspace version in `Cargo.toml`.
- **Linux binary checks in the release build job** (before packaging):
  `file` reports statically linked, `ldd` reports not a dynamic executable,
  and `ship --version` runs inside a `busybox` container with
  `--network none` (no Rust, Node.js or libraries there).
  `scripts/no-network-smoke.sh` runs every Phase 0 command that needs no
  root under `strace -f`: `--version`, `--help`, `target add/list/use`,
  `daemon` on a temp state dir and socket, `projects -o json` over the
  local socket, `dial-stdio` with stdin closed. It fails on any `connect`,
  `sendto` or `sendmsg` to an `AF_INET` / `AF_INET6` address. It runs on
  both Linux arches. On macOS, the arm64 binary runs `--version`.
- **Workflow triggers:** `release.yml` runs on `push: tags: v*` (build,
  check, sign, verify, publish), and on `workflow_dispatch` for a dry run
  that builds, checks, writes the SBOM and checksums, and skips signing and
  publishing. `id-token: write` and `contents: write` are granted only to
  the sign/publish job.
- **In-workflow verification:** before publishing, the publish job runs
  `scripts/install.sh --from <dir> --version <tag>` with real cosign
  against the files it just signed. It must install. Then it runs the
  script on a copy with one byte of an archive changed, and on a copy whose
  `SHA256SUMS` bundle belongs to another file. Both must be refused.
- **`install.sh`** (POSIX `sh`, `set -eu`): options `--version vX.Y.Z`
  (default: the latest release, resolved through the
  `releases/latest` redirect), `--prefix DIR` (default `/usr/local/bin`,
  else `$HOME/.local/bin` when that is not writable and not root),
  `--from DIR` (use release files already on disk; needs `--version`). It
  works in a fresh `mktemp -d` that it removes on exit. Order: download →
  cosign verifies `SHA256SUMS` → the archive's line must exist exactly
  once in `SHA256SUMS` → `sha256sum` / `shasum -a 256` must match → extract
  → `ship` must be a regular file → `install -m 0755`. The script itself
  never runs as or asks for root. Only the final `install` is prefixed
  with `sudo` when the prefix is not writable, and only after
  verification. Platform from `uname -s` / `uname -m`; anything else is
  refused. Exit 0 installed, 1 refused or failed, 2 usage.
- **Dependency audit (CI job `audit`):** `cargo deny check` with `deny.toml`
  (advisories: any vulnerability fails, unmaintained and yanked fail;
  licences: an allowlist of permissive licences; sources: crates.io only;
  bans: wildcard versions denied). `npm audit --audit-level=low` in `web/`.
  `scripts/web-licenses.mjs` fails on any package in
  `web/package-lock.json` whose `license` is missing or not in the
  allowlist. The job then proves the audit can fail: `cargo deny check
  advisories` on `scripts/audit-fixture/`, a crate with no source of its
  own that depends on `smallvec = "=1.6.0"` (RUSTSEC-2021-0003), must exit
  non-zero.
- **Install script test (CI job `install-script`, ubuntu and macOS):**
  `shellcheck` on the new scripts (ubuntu), then
  `scripts/install-selftest.sh`. It builds a fake release directory for the
  host platform, with a stub `cosign` first on `PATH` that accepts only a
  bundle whose content is the SHA-256 of the signed file.

## Declared file scope

- **owned_paths:**
  - .github/workflows/release.yml
  - .github/workflows/ci.yml
  - scripts/install.sh
  - scripts/install-selftest.sh
  - scripts/no-network-smoke.sh
  - scripts/web-licenses.mjs
  - scripts/audit-fixture/
  - deny.toml
  - README.md
  - CONTRIBUTING.md
  - docs/release.md
  - .metis/briefs/phase-0-ws-0.9.md
- **read_only_paths:** OVERVIEW.md, .metis/adr/, .metis/plans/phase-0-plan.md, crates/, web/, Cargo.toml, Cargo.lock, Makefile, scripts/service-smoke.sh

## Definition of Done

- [ ] `release.yml` on a `v*` tag builds the four archives with the UI
      embedded (`SHIP_REQUIRE_UI=1`) and publishes them, `SHA256SUMS`,
      the `.sigstore.json` bundles, the SBOM and `install.sh` to a GitHub
      release. A tag that differs from the workspace version fails first.
- [ ] Both Linux binaries pass the static checks, run `--version` in a
      `busybox` container with no network, and pass
      `scripts/no-network-smoke.sh`.
- [ ] `install.sh` installs a good release. It refuses, installs nothing,
      and exits 1 when the archive's checksum does not match, when the
      archive is missing from `SHA256SUMS`, when the signature does not
      verify, and when `cosign` is absent. `install-selftest.sh` checks
      every case in CI. The publish job checks the good case and both
      tamper cases with real cosign.
- [ ] CI job `audit` runs cargo-deny (advisories, licences, sources, bans),
      `npm audit` and the web licence check on every push and PR. Its
      self-test shows `cargo deny` fails on a known vulnerability.
- [ ] README lists exactly the platforms CI builds and tests, and the
      verifying install steps (download, inspect, run; never `curl | sudo
      sh`).
- [ ] No real hostname, address or personal name added.
- [ ] `metis verify --post` green.

## Test plan

- `scripts/install-selftest.sh` (CI `install-script`, ubuntu + macOS):
  good install; tampered archive; missing checksum line; bad signature;
  no cosign; unknown option exits 2. Each refusal leaves the prefix empty.
- Publish job: `install.sh --from` with real cosign on the signed release,
  good plus two tamper cases.
- Release build jobs: static-link checks, busybox `--network none` run,
  `scripts/no-network-smoke.sh` under strace (linux amd64 and arm64).
- CI `audit`: cargo-deny, npm audit, web licences, and the vulnerable
  fixture must fail.
- Gate: `metis verify --post` (`make verify`; none of the new files are in
  the Rust build).
- Not testable here (macOS dev machine, no tag pushed): the release
  workflow end to end. It runs on the first tag or a `workflow_dispatch`
  dry run.

## Out-of-scope touches

None.

## Report

Filled at implementation.
