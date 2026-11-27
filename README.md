# ship

One binary that turns a folder on your laptop into a running, observable
application on a Linux machine you own, without Kubernetes, a cluster or a
hosted account. See [OVERVIEW.md](OVERVIEW.md) for the full specification.

## Status

**Pre-alpha.** Nothing deploys yet. Phase 0 is the foundation: `ship
daemon`, `ship service install`, `ship target`, `ship host` (host and
Docker status), `ship projects` (an empty registry), `ship open` (the UI
shell) and `ship --version`.

- Before v1.0 the CLI, the HTTP API and the storage formats may change
  without notice. Semantic versioning applies from v1.0.
- The name "Ship" has not yet been checked for conflicts and may change.

## Platforms

Only what CI builds and tests is claimed:

| Platform     | Role             | CI                                                                                                                                                                  |
|--------------|------------------|---------------------------------------------------------------------------------------------------------------------------------------------------------------------|
| linux amd64  | daemon and client | build, lint, unit tests; real Docker Engine, real sshd and real systemd tests; static release build run with no libraries and no network; `install.sh` self-test |
| linux arm64  | daemon and client | static release build run with no libraries and no network                                                                                                           |
| macOS arm64  | client           | release build, `ship --version`; `install.sh` self-test                                                                                                             |
| macOS amd64  | client           | release build only (cross-compiled; not run in CI)                                                                                                                  |

`ship daemon` and `ship service` run on Linux only. Further platforms are
added here when CI covers them.

## Footprint

One measurement, taken at the Phase 0 gate, not a guarantee: on Ubuntu,
x86_64, a static musl build, about 3 minutes after start with no projects,
the idle daemon held 5.7 MB resident memory (VmRSS 5716 kB, peak the
same), ran 10 threads and had used 0.21 s of CPU in total since start.
`ship projects` over ssh on a LAN took 0.12–0.13 s per run with an
already-open ssh connection; a bare `ssh HOST true` took 0.12 s on the same
link.

## Install

Each release carries four archives, `SHA256SUMS`, a Sigstore signature
bundle (`*.sigstore.json`) for every file, an SPDX SBOM and `install.sh`.
The script verifies the signature (with [cosign](https://docs.sigstore.dev/cosign/system_config/installation/),
which must be installed, 2.4 or later) and the SHA-256 before it installs
anything, and refuses otherwise. Download it, read it, then run it as
yourself. Do not pipe it into a root shell:

```sh
curl -fsSLO https://github.com/techspeque/ship/releases/latest/download/install.sh
less install.sh
sh install.sh                       # or: sh install.sh --version v0.0.1 --prefix ~/bin
```

It installs to `/usr/local/bin` when that is writable, otherwise
`~/.local/bin`. With an explicit `--prefix` that is not writable, only the
final copy uses `sudo`. To verify by hand instead, see
[docs/release.md](docs/release.md).

## Build

```sh
make verify          # cargo fmt --check, clippy -D warnings, cargo test
cargo run -p ship -- --version
```

The Rust toolchain is pinned in `rust-toolchain.toml`; rustup installs it on
first use. See [CONTRIBUTING.md](CONTRIBUTING.md).

## Security

Report vulnerabilities privately as described in [SECURITY.md](SECURITY.md).
Ship has no hosted service and sends no telemetry.

## Licence

MIT. See [LICENSE](LICENSE).
