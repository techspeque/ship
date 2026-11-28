# ship

One binary that turns a folder on your laptop into a running, observable
application on a Linux machine you own, without Kubernetes, a cluster or a
hosted account. See [OVERVIEW.md](OVERVIEW.md) for the full specification.

## Status

**Pre-alpha.** Phase 1 (Push) is built and awaits its adoption gate. On
top of the Phase 0 foundation (`ship daemon`, `ship service install`,
`ship target`, `ship host`, `ship open`, `ship --version`) it has:

- `ship push [PATH]`: the working tree, uncommitted changes included, sent
  as a snapshot, built from its Dockerfile or imported from its Compose
  file under the trust profile, started and verified, with `--port` to
  publish on the host's publish address (`ship settings get|set
  publish.address`; the tailnet address under `auto` when there is one,
  else `127.0.0.1`). `--wait -o json` is the machine contract in
  [docs/contracts/cli-json.md](docs/contracts/cli-json.md).
- `ship projects`, `ship status`, `ship deployments` and `ship logs`, with
  desired state, the last deployment and what Docker reports kept apart.
- `ship start|stop|restart PROJECT[/SERVICE]`, `ship redeploy|remove
  PROJECT`; a removed environment keeps its volumes and history and comes
  back with `ship push --recreate`.
- `ship vars` and write-only `ship secrets`, injected at deploy.
- A UI with projects, services, live operation logs and history.
- Recovery: a daemon that dies mid-operation starts again with no Docker
  action. What it left in flight is `interrupted`, never resumed. A
  deployment cut short while building or preparing changed nothing; one
  cut short while deploying or verifying may have replaced containers in
  part, and `ship status` says so and names the command that puts it right
  (`ship redeploy PROJECT`). See [docs/service.md](docs/service.md).

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

One measurement, taken at the Phase 0 gate (run 2, 2026-11-27), not a
guarantee: the v0.0.2 release build under systemd on Ubuntu 26.04, x86_64,
16 cores, 16 minutes after start with no projects. The idle daemon held
6.3 MB resident memory (VmRSS 6448 kB, peak 6508 kB), ran 18 threads, used
no measurable CPU over 60 idle seconds (0 ticks at 100 Hz) and 0.02 s of
CPU in total since start. `ship projects` over ssh on a LAN took
0.11–0.14 s per run, each run opening a new ssh connection; a bare
`ssh HOST true` took 0.12 s on the same link, so almost all of it is the
ssh handshake.

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
final copy uses `sudo`. A copy under your home directory is fine for the
client; the daemon needs a system path, so on a Linux host that runs it,
install.sh prints the `sudo install … /usr/local/bin/ship` and
`sudo /usr/local/bin/ship service install` commands, and
`ship service install` refuses a binary the `ship` user cannot run. See
[docs/service.md](docs/service.md). To verify by hand instead, see
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
