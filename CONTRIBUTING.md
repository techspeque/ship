# Contributing

Ship is pre-alpha; expect large changes. Read [OVERVIEW.md](OVERVIEW.md) and
the ADRs in `.metis/adr/` before proposing a design change.

## Build and test

Requirements: `rustup`, `make` and Node.js with npm (the version in
`web/.nvmrc`; any Node `^20.19` or `>=22.12` works). The toolchain is pinned
in `rust-toolchain.toml` and installed by rustup on first use. Node builds
the UI only; the `ship` binary never needs it at runtime.

```sh
make verify                     # the gate CI runs; must pass before a merge
make web                        # build the UI into web/dist
cargo fmt --all                 # format
cargo build                     # debug build of the whole workspace
cargo test --workspace          # all tests
cargo run -p ship -- --version  # run the binary
```

`make verify` runs, in order:

1. `make web`: in `web/`, `npm ci`, `npm run lint`, `npm run check`,
   `npm test` and `npm run build`
2. `cargo fmt --all --check`
3. `cargo clippy --workspace --all-targets -- -D warnings`
4. `cargo test --workspace`

## The UI

`web/` is Svelte 5, TypeScript, Vite and Tailwind. `npm run build` writes
`web/dist`, which `crates/ship-daemon/build.rs` embeds in the binary. A
daemon built without `web/dist` still compiles and answers UI paths with
`503 UI not built`; `SHIP_REQUIRE_UI=1` turns that into a build error, for
releases. Dependencies are pinned exactly; commit `web/package-lock.json`
with any change to them. The UI loads nothing from another origin: no
fonts, CDNs or other remote URLs, which the daemon's CSP enforces.

`web/src/lib/api.ts` holds hand-written TypeScript types for the API.
They are kept in step with the Rust types by fixtures in
`web/src/lib/contract/`: `crates/ship-daemon/tests/web_contract.rs` fails
when the Rust types no longer serialise to them, and
`web/src/lib/contract.test.ts` fails when `api.ts` no longer accepts them.
After an intended change to an API type:

```sh
SHIP_UPDATE_CONTRACT=1 cargo test -p ship-daemon --test web_contract
cd web && npm test   # then update api.ts until this passes
```

`cd web && npm run dev` serves the UI with hot reload against a daemon on
`127.0.0.1:7180`.

## Dependency audit

CI job `audit` runs `cargo deny check` against [deny.toml](deny.toml)
(RustSec advisories, licences, sources, bans), `npm audit` in `web/` and
`node scripts/web-licenses.mjs` (the web licence allowlist). Any known
vulnerability or unlisted licence fails the job. A new dependency with a
licence outside the allowlist needs the allowlist changed in both files,
in the same pull request, with the reason. Locally, with `cargo-deny`
installed:

```sh
cargo deny --locked check
cd web && npm audit && cd .. && node scripts/web-licenses.mjs
```

## Releases

See [docs/release.md](docs/release.md).

## Workspace layout

The crates and the dependency rule are in OVERVIEW §2.3. `ship` and
`ship-daemon` may depend on any workspace crate; `ship-core` depends on none;
every other crate is a leaf and depends only on `ship-core`.
`crates/ship/tests/dependency_rule.rs` fails the build when this is broken.

## Rules for changes

- Keep real hostnames, IP addresses, personal names and credentials out of
  the repository, including tests and examples. Use documentation domains
  such as `example.com` and addresses from `192.0.2.0/24`.
- Commit `Cargo.lock` and `web/package-lock.json` with any dependency
  change.
- Report security issues privately, as described in [SECURITY.md](SECURITY.md).

By contributing you agree that your contribution is licensed under the MIT
licence in [LICENSE](LICENSE).
