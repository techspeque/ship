# Contributing

Ship is pre-alpha; expect large changes. Read [OVERVIEW.md](OVERVIEW.md) and
the ADRs in `.metis/adr/` before proposing a design change.

## Build and test

Requirements: `rustup` and `make`. The toolchain is pinned in
`rust-toolchain.toml` and installed by rustup on first use.

```sh
make verify                     # the gate CI runs; must pass before a merge
cargo fmt --all                 # format
cargo build                     # debug build of the whole workspace
cargo test --workspace          # all tests
cargo run -p ship -- --version  # run the binary
```

`make verify` runs, in order:

1. `cargo fmt --all --check`
2. `cargo clippy --workspace --all-targets -- -D warnings`
3. `cargo test --workspace`

## Workspace layout

The crates and the dependency rule are in OVERVIEW §2.3. `ship` and
`ship-daemon` may depend on any workspace crate; `ship-core` depends on none;
every other crate is a leaf and depends only on `ship-core`.
`crates/ship/tests/dependency_rule.rs` fails the build when this is broken.

## Rules for changes

- Keep real hostnames, IP addresses, personal names and credentials out of
  the repository, including tests and examples. Use documentation domains
  such as `example.com` and addresses from `192.0.2.0/24`.
- Commit `Cargo.lock` with any dependency change.
- Report security issues privately, as described in [SECURITY.md](SECURITY.md).

By contributing you agree that your contribution is licensed under the MIT
licence in [LICENSE](LICENSE).
