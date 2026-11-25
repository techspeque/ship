# ship

One binary that turns a folder on your laptop into a running, observable
application on a Linux machine you own, without Kubernetes, a cluster or a
hosted account. See [OVERVIEW.md](OVERVIEW.md) for the full specification.

## Status

**Pre-alpha.** Nothing deploys yet: the repository holds the workspace
scaffold, and `ship --version` is the only working command.

- Before v1.0 the CLI, the HTTP API and the storage formats may change
  without notice. Semantic versioning applies from v1.0.
- The name "Ship" has not yet been checked for conflicts and may change.

## Platforms

Only what CI builds and tests is claimed:

| Platform     | CI                        |
|--------------|---------------------------|
| linux amd64  | build, lint, unit tests   |

Further platforms are added here when CI covers them.

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
