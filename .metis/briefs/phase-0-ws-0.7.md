# phase-0-ws-0.7 — UI shell

- **Type:** feat | **Risk:** medium | **Priority:** p2
- **Coder:** claude-code/opus-5-5 | **Date:** 2026-11-25
- **Plan:** .metis/plans/phase-0-plan.md §0.7

## Goal

The daemon serves an embedded Svelte UI that shows host status from
`/api/v1/host` (Docker `ready`, `unreachable` and `unsupported_version` shown
differently) and the empty projects list. Its TypeScript types cannot drift
from the Rust types, and `ship open` brings the UI up for a local or ssh
target.

## Architectural context

- OVERVIEW §2.3 (`web/` built to static files and embedded; `ship-daemon`
  serves the UI), §3.1 (Svelte + TypeScript + Vite + Tailwind, no Node.js at
  runtime), §3.3 (listeners only on the unix socket and 127.0.0.1; the CLI
  never prints an address that does not exist), §5 (no login page; a same-origin
  check on mutations), §6 CLI (`ship open PROJECT`), §7 UI testing.
- ADR-0003 rule 6 (same-origin guard, already in `http.rs`; the Host guard
  already accepts any port on a loopback name for the ssh forward). ADR-0012
  (no telemetry, no third-party origin).
- Consumes `ship-core::api` (`HostInfo`, `DockerInfo`, `DockerStatus`,
  `HostCapacity`, `SupportedVersions`, `ProjectList`, `ProjectSummary`,
  `API_VERSION_HEADER`, paths), `ship-daemon::http::router`, `ship-net`
  (`Target`, `SshTarget`, `connect_unix`), and the `ship` client
  (`commands::endpoint`, `client::get_json`).
- No new Rust crates: assets are embedded by a `build.rs` in `ship-daemon`
  that generates `include_bytes!` entries for `web/dist`.

Design decisions:
- **web/:** Svelte 5, TypeScript, Vite, Tailwind v4 (`@tailwindcss/vite`),
  Vitest, ESLint (flat config, `typescript-eslint`, `eslint-plugin-svelte`),
  `svelte-check`. Exact versions (`save-exact`), `package-lock.json`
  committed, installed with `npm ci`. System font stack, no fonts, CDNs or
  other remote URLs.
- **Page:** a header, a host card and a projects section. The host card covers
  loading, request failure (with the daemon's `ApiError` message when there
  is one), and the three Docker states, each with its own label, colour and
  icon text, the versions, the supported ranges and the daemon's message, plus
  capacity (an unknown value shows as "unknown"). Projects: a list, or "No
  projects yet." Polls every 10 s, and has a Refresh button. Requests send
  `Ship-Api-Version: v1`. Light and dark themes follow `prefers-color-scheme`
  (Tailwind `dark:`). Everything is reachable by Tab with a visible focus ring;
  there is a skip link to the main content, and status changes go through an
  `aria-live` region.
- **View logic** (`web/src/lib/view.ts`) is pure TS: `HostInfo` → view model
  (label, tone, detail lines), and byte/CPU formatting. It is unit-tested.
  Components only render it.
- **Type drift guard:** `web/src/lib/api.ts` holds the hand-written TS types
  and exact-shape runtime guards (`isHostInfo`, `isProjectList`: unknown or
  missing keys and wrong types are rejected). A Rust test
  (`crates/ship-daemon/tests/web_contract.rs`) builds struct-literal samples
  covering every field and every `DockerStatus` variant, serialises them and
  compares them with committed fixtures in `web/src/lib/contract/`.
  `SHIP_UPDATE_CONTRACT=1` rewrites the fixtures. A Vitest test runs the guards
  on every fixture and checks the TS `DockerStatus` union against the
  fixture's list of statuses. So a changed Rust type fails the Rust test until
  the fixture is updated, and the updated fixture fails the TS test until
  `api.ts` matches. The UI also uses the guards at runtime, so a mismatched
  response shows as an error and is never rendered wrongly.
- **Embedding (`ship-daemon/build.rs`):** walks `../../web/dist` and generates
  `ASSETS: &[(path, bytes, content_type)]` in `OUT_DIR`, with
  `rerun-if-changed` on the directory. If `web/dist` is missing, it embeds
  nothing and the UI routes serve a plain-text 503 "UI not built: run `make
  web`". Setting `SHIP_REQUIRE_UI=1` turns a missing `dist` into a build
  error, for release builds (used by 0.9).
- **Serving (`ship-daemon/src/ui.rs`, wired as the router's fallback):** `GET`
  and `HEAD` only. `/api/*` keeps its JSON 404. An exact asset path is served
  with its content type. `/assets/*` gets `Cache-Control: public,
  max-age=31536000, immutable`, and a missing `/assets/*` file is 404. Any
  other path falls back to `index.html` with `Cache-Control: no-cache`. Every
  UI response carries `Content-Security-Policy: default-src 'none';
  script-src 'self'; style-src 'self'; img-src 'self' data:; connect-src
  'self'; font-src 'self'; base-uri 'none'; form-action 'none';
  frame-ancestors 'none'`, `X-Content-Type-Options: nosniff` and
  `Referrer-Policy: no-referrer`. A non-GET method on a UI path gets 405. The
  existing origin and Host guards run first, unchanged.
- **`ship open [--port N] [--no-browser] [--timeout SECS]`** (`PROJECT`
  arrives with projects in Phase 1):
  - local target: checks `http://127.0.0.1:N/healthz` (N defaults to 7180),
    then opens `http://127.0.0.1:N/`. If the check fails it exits 1, saying
    the daemon's 127.0.0.1 listener is not answering on N, and prints no URL.
  - ssh target: picks a free 127.0.0.1 port P and spawns `ssh -N -T -o
    BatchMode=yes -o ExitOnForwardFailure=yes -o ConnectTimeout=S [-p PORT]
    [-l USER] -L 127.0.0.1:P:<remote socket> -- HOST`. That is structured
    argv, no shell, `kill_on_drop`, built in `ship-net::SshTarget::forward_args`
    and forwarding to the daemon's unix socket, so no remote port is assumed.
    It waits for `/healthz` through P (bounded by `--timeout`), then opens
    `http://127.0.0.1:P/` and stays in the foreground until Ctrl-C or ssh
    exits. On Ctrl-C (SIGINT/SIGTERM) it kills and reaps ssh, which closes
    the forward.
  - browser: `open` (macOS) or `xdg-open` (others), with the URL as one
    argument. `--no-browser` prints the URL only.
- **make verify:** `verify: web fmt clippy test`. `web` runs `npm ci`,
  `npm run lint`, `npm run check`, `npm test` and `npm run build` in `web/`,
  before cargo, so the binary embeds the fresh build. CI's verify job installs
  the Node version pinned in `web/.nvmrc`.

## Declared file scope

- **owned_paths:**
  - web/
  - crates/ship-daemon/build.rs
  - crates/ship-daemon/Cargo.toml
  - crates/ship-daemon/src/ui.rs
  - crates/ship-daemon/src/http.rs
  - crates/ship-daemon/src/lib.rs
  - crates/ship-daemon/tests/ui.rs
  - crates/ship-daemon/tests/web_contract.rs
  - crates/ship/Cargo.toml
  - crates/ship/src/
  - crates/ship/tests/cli_open.rs
  - crates/ship/tests/ssh_real.rs
  - crates/ship-net/src/target.rs
  - crates/ship-net/src/lib.rs
  - Makefile
  - .github/workflows/ci.yml
  - CONTRIBUTING.md
  - Cargo.lock
  - .metis/briefs/phase-0-ws-0.7.md
- **read_only_paths:** OVERVIEW.md, .metis/adr/, .metis/plans/phase-0-plan.md, Cargo.toml, crates/ship-core/, crates/ship-store/, crates/ship-docker/, crates/ship-net/ (other files), crates/ship-daemon/ (other files), crates/ship/tests/ (other files)

## Definition of Done

- [ ] `web/` builds with `npm ci && npm run build`. Dependencies are pinned exactly and `package-lock.json` is committed.
- [ ] A daemon built after `make web` serves `index.html` at `/` and at a client route such as `/projects/x`, serves its hashed assets with the right content types, and serves no remote URL in `index.html`. Every UI response carries the CSP above. `/api/nope` is still a JSON 404, and a missing `/assets/x.js` is 404.
- [ ] Built without `web/dist`, the daemon compiles and answers UI paths with the 503 "UI not built" text.
- [ ] View-model tests show `ready`, `unreachable` and `unsupported_version` with different labels and tones, plus a loading state, an error state and an empty projects state.
- [ ] Changing a Rust API type (for example adding a field to `HostInfo`) fails `make verify`: first the Rust contract test, then, once the fixtures are regenerated, the TS contract test.
- [ ] `ship open --no-browser` on a local target prints `http://127.0.0.1:N/` only after `/healthz` answers. With nothing listening it exits 1 and prints no URL.
- [ ] `ship open --no-browser` on an ssh target (fake `ssh` that runs the forward) prints a 127.0.0.1 URL that serves the UI. SIGINT ends `ship` and leaves no `ssh` process. The recorded ssh argv equals the expected list, with HOST as one argument after `--`. The ignored `ssh_real` test does the same against the real sshd in CI.
- [ ] The CSS contains a `prefers-color-scheme: dark` rule, and the interactive controls are native buttons and links with focus-visible styles.
- [ ] `make verify` runs the web lint, type check, unit tests and build.
- [ ] `metis verify --post` is green.

## Test plan

- `web/src/lib/view.test.ts`: the view model for each Docker state, the loading, error and empty states, and formatting.
- `web/src/lib/contract.test.ts`: guards accept every Rust fixture; guards reject an extra key, a missing key, a wrong type and an unknown status; the TS status list equals the fixture's.
- `crates/ship-daemon/tests/web_contract.rs`: serialised samples equal the committed fixtures (`SHIP_UPDATE_CONTRACT=1` rewrites them).
- `crates/ship-daemon/tests/ui.rs`: index at `/` and on client routes, asset types and cache headers, missing asset 404, `/api` 404 unchanged, CSP and nosniff headers, 405 on POST to a UI path, no `http://` or `https://` in `index.html` src/href. Skipped with a message when built without the UI. `ui.rs` unit tests cover path resolution and the not-built 503.
- `crates/ship-net/src/target.rs`: exact `forward_args` argv, including a port and a user.
- `crates/ship/tests/cli_open.rs`: local success and failure with an in-process daemon on a free port; ssh through a fake `ssh` (Python 3) that records argv and forwards the local port to the daemon socket, then GET of the printed URL, SIGINT, and no child left.
- `crates/ship/tests/ssh_real.rs`: `ship open --no-browser` over the real sshd: GET the UI, SIGINT, forward closed (ignored; CI `ssh` job).
- Gate: `metis verify --post` (`make verify`).

## Out-of-scope touches

None.

## Report

- Earlier session blocked on npm being refused; cleared (5e4bafb), npm now
  runs.
- Versions: Svelte 5.59.1, @sveltejs/vite-plugin-svelte 7.3.2, Vite 8.3.8,
  Tailwind 4.3.5, TypeScript 6.0.3, Vitest 4.1.12, ESLint 10.15.0,
  typescript-eslint 8.74.0, eslint-plugin-svelte 3.24.1, svelte-check 4.7.8.
  Vitest 4 rather than 5: Vitest 5 needs Node >= 22.12 and the local
  machine runs Node 20.19.3. TypeScript 6 rather than 7: typescript-eslint
  supports `<6.1.0`. `engines` is `^20.19.0 || >=22.12.0`; `web/.nvmrc`
  pins 24.11.1 for CI.
- Exact pins are in `package.json` itself; a `web/.npmrc` with
  `save-exact=true` was refused by this session's permissions (sensitive
  file), so it is not committed.
- In this session `cargo`, `make` and `rustfmt` are refused ("requires
  approval"); only `metis verify` runs them. So the contract fixtures were
  written by hand in `serde_json::to_string_pretty` form and checked by
  `web_contract.rs` under `metis verify`, not produced with
  `SHIP_UPDATE_CONTRACT=1`. The update path is implemented but was not run
  here.
- Extra fixture beyond the brief's list: `error-not-found.json` with an
  `isApiError` guard, since the UI parses the daemon's `ApiError`.
- `ship open`: `--port` also sets the local end of the ssh forward when
  given (default: a free port). Ctrl-C exits 0; ssh exiting on its own
  exits 1. `ssh_real.rs` accepts 200 (UI built) or the 503 not-built text,
  since CI's `ssh` job does not run `make web`.
- `metis verify --post`: green.
