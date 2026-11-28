# phase-1-ws-1.4 — Snapshot pack

- **Type:** feat | **Risk:** high | **Priority:** p2
- **Coder:** claude-code/opus-5-5 | **Date:** 2026-11-27
- **Plan:** .metis/plans/phase-1-plan.md §1.4

## Goal

Implement the client side of ADR-0005 in `ship-source`: select a working
tree's files under its ignore rules and default secret exclusions, and
stream them as a deterministic tar+zstd snapshot with a SHA-256 of the
stream in bounded memory, with the `--dry-run` report and the home, root
and size refusals.

## Architectural context

- ADR-0005 rules 1, 2, 3, 4, 6, 8, 9; OVERVIEW §2.3 (`ship-source` is a
  leaf, depends only on `ship-core`), §3.3 (a snapshot never contains
  `.git/`, `.env`, `.env.*` other than templates, or private keys unless
  the operator names them).
- Unpack, upload and the daemon side are phase-1-ws-1.5; the `ship push`
  flag wiring is phase-1-ws-1.9. This slice is a library API only.
- New in `ship-source` (`crates/ship-source/src/`):
  - `Limits` { max_files, max_bytes } with conservative defaults;
    `Options` { include (operator-named relative paths), limits, home
    (override for tests; default `$HOME`) }.
  - `scan(root, &Options) -> Result<Selection, ScanError>`: own directory
    walk, sorted by byte-wise path, never following symlinks. Per
    directory, `.gitignore` then `.shipignore` lines are compiled into one
    gitignore matcher (later lines win, so `.shipignore` overrides);
    deepest matcher decides, ignored directories are not descended. No
    Git metadata, global excludes or `.git/info/exclude` are read.
    Default exclusions are checked before ignore rules, so a sensitive
    file is reported even when it is also ignored.
  - Default exclusions (`Exclusion` kind): `git_metadata` (`.git`),
    `env_file` (`.env`, `.env.*` except `.env.example`, `.env.sample`,
    `.env.template`, `.env.dist`), `private_key` (`id_rsa`, `id_dsa`,
    `id_ecdsa`, `id_ed25519` and `*.pem`, `*.key`, `*.p12`, `*.pfx`,
    `*.jks`, `*.keystore`), `credential_store` (`.netrc`, `_netrc`,
    `.pgpass`, `.git-credentials`, `.npmrc`, `.pypirc`, and directories
    `.ssh`, `.aws`, `.gnupg`, `.kube`, `.docker`), `build_directory`
    (`node_modules`, `target`). A path in `Options::include`, or under
    one, is kept. Include paths must be relative and free of `..`.
  - `.dockerignore` is an ordinary file; never applied.
  - Skipped (reported, never packed): symlinks, special files (FIFO,
    socket, device), names that are not UTF-8.
  - `Selection`: sorted entries (dirs and regular files, relative `/`
    paths, size, executable bit), `excluded` (path + kind), `skipped`
    (path + reason), `file_count`, `total_bytes`; `report()` gives the
    serialisable `DryRunReport` (file count, size, every default
    exclusion, every skipped entry).
  - Refusals (`ScanError`): root is a filesystem root, the home directory
    or an ancestor of it; file count or total size over `Limits`; not a
    directory; I/O errors with the path.
  - `pack(&Selection, writer) -> Result<Packed, PackError>`: tar (GNU
    headers, mtime 0, uid/gid 0, no user/group names, mode 0755 for dirs
    and executables else 0644) → zstd (level 3, single-threaded) → a
    hashing writer → the caller's writer. Files are streamed with
    `io::copy` through a fixed buffer; nothing is held whole. A file whose
    length changed since the scan fails the pack. `Packed` { sha256 (64
    lowercase hex), compressed_bytes }.
- New dependencies of `ship-source`: `ignore` (gitignore matcher only),
  `tar` (default features off), `zstd`, `sha2` (0.11, already in the
  graph), `serde`. Dev: `tempfile`, `libc` (both already in the graph).
  `zstd-sys` compiles bundled C, as SQLite does under musl-gcc.

## Declared file scope

- **owned_paths:**
  - crates/ship-source/
  - Cargo.lock
  - .metis/briefs/phase-1-ws-1.4.md
- **read_only_paths:** .metis/adr/, .metis/plans/phase-1-plan.md,
  OVERVIEW.md, docs/contracts/cli-json.md, crates/ship-core/, deny.toml

## Definition of Done

- [ ] An uncommitted (modified) and an untracked file are in the snapshot
      of a Git working tree; a file ignored by `.gitignore` or
      `.shipignore` is not; `.shipignore` can re-include with `!`;
      nested ignore files apply to their subtree; a tree without `.git`
      packs the same way.
- [ ] `.git/`, `.env`, `.env.local` and a private key are absent by
      default and listed in the report; `.env.example` is present; a path
      named in `include` is present.
- [ ] `.dockerignore` is in the snapshot and its patterns are not applied.
- [ ] The same tree packs to the same hash twice, and after its mtimes
      change.
- [ ] Packing 64 MiB of incompressible data grows peak RSS by less than
      32 MiB.
- [ ] A symlink (to a file, a directory, and outside the tree) is never
      followed and is reported as skipped.
- [ ] A filesystem root, the home directory and a tree over the file-count
      or byte limit are refused before anything is packed.
- [ ] `ship-source` depends on no workspace crate other than `ship-core`.
- [ ] `metis verify --post` green.

## Test plan

- Unit (`exclude.rs`): each default exclusion kind; `.env.example` and
  the other templates kept; `.pub` keys kept; include matching (exact,
  under, not a prefix of a sibling name); include validation.
- `crates/ship-source/tests/pack.rs` (temp dirs, real files): git-like
  tree (with a real `git` repo when `git` is on PATH, otherwise a `.git`
  dir) with modified, untracked and ignored files; `.shipignore`
  override and negation; nested ignore; `.dockerignore` sent and not
  applied; default exclusions absent and reported, include restores;
  symlinks skipped; FIFO skipped; determinism across two packs and an
  mtime change; the archive decodes (zstd + tar) to exactly the selected
  entries with their content, and the digest equals SHA-256 of the
  bytes written; home, root and limit refusals; changed-file detection.
- `crates/ship-source/tests/memory.rs` (one test, own process): peak RSS
  growth via `getrusage` while packing 64 MiB of pseudo-random files to
  a sink stays under 32 MiB.
- `crates/ship-source/tests/deps.rs`: the manifest names no workspace
  crate but `ship-core`.

## Out-of-scope touches

None.

## Report

- `src/exclude.rs` (new): `Exclusion`, `classify(name, is_dir)`; the
  lists in the brief, matched ASCII case-insensitively.
- `src/scan.rs` (new): `Limits` (defaults 50 000 entries, 512 MiB),
  `Options`, `scan`, `Selection`, `Entry`, `EntryKind`, `Excluded`,
  `Skipped`, `SkipReason`, `DryRunReport` (serde + `Display`),
  `ScanError`, `IGNORE_FILES`. Iterative walk (no recursion depth
  limit); limits checked as entries are added, so an oversize tree stops
  early.
- `src/pack.rs` (new): `pack`, `Packed`, `PackError`, `ZSTD_LEVEL`.
  Files opened with `O_NOFOLLOW | O_NONBLOCK` and re-checked by `fstat`
  (regular file, scanned size); an exact-length reader fails a shrunk
  file and a one-byte probe fails a grown one, both as `Changed`.
- `src/lib.rs`: modules and re-exports.
- `Cargo.toml`: `ignore`, `serde`, `sha2`, `tar` (no default features),
  `zstd` (no default features); `libc` under `cfg(unix)`; dev
  `serde_json`, `tempfile`. `Cargo.lock` adds aho-corasick, bstr,
  const-oid, crossbeam-deque, crossbeam-epoch, filetime, globset, ignore,
  jobserver, regex-automata, regex-syntax, same-file, tar, walkdir,
  winapi-util, zstd, zstd-safe, zstd-sys (all MIT, Apache-2.0 or
  Unlicense dual licences). `cargo deny` was not run locally; CI's
  `audit` job checks it.
- Tests: 13 unit (`exclude.rs`, `scan.rs`, `pack.rs`), `tests/pack.rs`
  (18), `tests/memory.rs` (1), `tests/deps.rs` (1).
- Decisions:
  - Entry order: a directory's entries in byte order, then each
    subdirectory's (parents precede children).
  - A named include lifts the block on that path and everything under
    it (default exclusions and ignore rules); a sibling it is only a
    string prefix of is not covered. Its blocked ancestors are entered
    restricted (only the way to the named path is sent) and still
    reported as excluded. It also overrides ignore rules,
    since `.env.*` files are usually gitignored. An include that matches
    nothing sendable is `IncludeNotFound`.
  - A file that is tracked by Git but matches `.gitignore` is excluded:
    with no Git metadata read, the ignore files are the only rule.
  - Ancestors of the home directory are refused as well as the home
    directory itself and filesystem roots.
  - `ship-core` is not a dependency yet; nothing from it is needed.
- `metis verify --post --slice phase-1-ws-1.4`: ALL GREEN (web, fmt,
  clippy, all workspace tests).
