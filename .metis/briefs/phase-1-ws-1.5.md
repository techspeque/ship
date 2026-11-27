# phase-1-ws-1.5 — Upload and safe unpack

- **Type:** feat | **Risk:** high | **Priority:** p2
- **Coder:** claude-code/opus-5-5 | **Date:** 2026-11-27
- **Plan:** .metis/plans/phase-1-plan.md §1.5

## Goal

Implement the daemon side of ADR-0005: an upload endpoint that streams a
tar+zstd snapshot into a new staging directory, validating every entry as
it arrives, enforcing size, entry-count and expansion-ratio limits during
the stream and checking the declared SHA-256 at the end, and promotes only
a fully valid stream to an immutable revision identified by its hash; a
failed stream leaves nothing but a failed operation record. The upload
travels over the local socket and over `ssh … ship dial-stdio`.

## Architectural context

- ADR-0005 rules 5, 7, 8, 9; ADR-0007 rule 1; ADR-0011 rules 1, 7;
  OVERVIEW §3.3 (unpacking never writes outside its own new staging
  directory; a revision is never modified; every mutation is a persisted
  operation before any side effect), §2.3 (`ship-source` is a leaf;
  `ship-net` holds all client transport).
- Consumed unchanged: the queue of phase-1-ws-1.3 (`Queue::submit`,
  `Context::enter`/`log`, `JobFailure`, `submitted_response`); the store
  (`get_or_create_project`, `list_revisions`, `create_revision`);
  `ship-core` error codes `payload_too_large` (413), `unsafe_snapshot`
  and `digest_mismatch` (422), `invalid_request`, `idempotency_conflict`;
  `RevisionKind::Snapshot`; the packer of phase-1-ws-1.4 (tests only).
- New in `ship-source` (`src/unpack.rs`, unix only):
  - `UnpackLimits` { max_compressed_bytes, max_bytes, max_entries,
    max_ratio, ratio_grace_bytes } with conservative defaults (512 MiB,
    512 MiB, 50 000, 100, 32 MiB).
  - `unpack(reader, staging, &UnpackLimits) -> Result<Unpacked,
    UnpackError>`: creates `staging` itself (0700, non-recursive, so it
    must not exist), reads the compressed stream through a hashing and
    counting reader, zstd (window at most 2^24), and its own tar header
    parser (no `tar` crate on this side: every header field is checked
    here, nothing is buffered beyond one 512-byte block and one GNU long
    name of at most 4 KiB). On any error the staging directory is removed
    before returning. `Unpacked` { sha256, compressed_bytes, entries,
    files, bytes }.
  - Accepted: regular files (`0`, NUL) and directories (`5`), and GNU
    long names (`L`) for the next entry. Rejected (`UnsafeReason`):
    absolute path, `..` or `.` or empty component, non-UTF-8 or control
    characters, component over 255 or path over 4096 bytes, symlink,
    hardlink (and GNU long link), character or block device, FIFO, PAX or
    any other type, an entry whose parent directory was not an earlier
    directory entry, an exact duplicate, and a path that equals an earlier
    one after NFD, lowercasing and NFC (Unicode and case collisions).
  - Limits checked as bytes arrive: compressed bytes, total file bytes
    (from each header before anything is written), entries, and expanded
    bytes ≤ max(ratio_grace_bytes, max_ratio × compressed so far) over
    the whole decompressed stream. After the end-of-archive block the rest
    of the stream must be zero bytes (at most 1 MiB) and end cleanly.
  - Files are created with `O_CREAT | O_EXCL | O_NOFOLLOW` and mode 0644
    (0755 when any execute bit is set), directories with a non-recursive
    `mkdir` at 0755, all under `staging` joined with validated components
    only.
  - `UnpackError` distinguishes `Unsafe`, `Malformed`, `TooLarge`,
    `Expansion`, `Transfer` (the source failed), `Staging` (a local write
    failed).
  - `ship_source::fuzzing::unpack_bytes(&[u8])` (doc-hidden): the fuzz
    body, shared by the cargo-fuzz target and a stable test.
- New in `ship-core` (`api.rs`): `uploads_path(slug)`
  (`/api/v1/projects/{slug}/uploads`), `SNAPSHOT_SHA256_HEADER`
  (`ship-snapshot-sha256`), `GIT_COMMIT_HEADER` (`ship-git-commit`),
  `GIT_DIRTY_HEADER` (`ship-git-dirty`), `UPLOAD_KIND` (`upload`),
  `UploadView` { operation_id, project_id, revision_id, sha256,
  new_revision }.
- New in `ship-daemon` (`src/upload.rs`, route in `http.rs`):
  - `POST /api/v1/projects/{project}/uploads`. Before any operation: the
    slug, `Ship-Snapshot-Sha256` (64 lowercase hex) and the optional git
    headers (commit 7–64 lowercase hex, dirty `true`|`false`) are checked,
    else `400`. Then `http`'s idempotency rule via `Queue::submit` (kind
    `upload`, no environment, so an upload never waits behind a deploy).
  - The job: enters `uploading`, logs the client's git commit and dirty
    flag as information only, gets or creates the project and its
    `development` environment, streams the request body over a bounded
    channel (8 chunks) into `unpack` on a blocking thread under
    `<state>/staging/<op id>`, enters `validating`, compares the hash,
    then under one promotion lock renames the staging directory to
    `<state>/snapshots/<sha256>` (or discards it when that directory
    already exists) and finds or creates the project's `snapshot`
    revision with `source_ref` = the hash. Any failure removes the
    staging directory; the operation ends `failed` with
    `payload_too_large`, `unsafe_snapshot`, `digest_mismatch`,
    `invalid_request` (broken transfer or bad archive) or `internal`.
  - The handler waits for the job and answers `201` (new revision) or
    `200` (existing revision) with `UploadView`, or the failure code's
    status with `operation_id`. A replayed key never reads the body: a
    succeeded upload answers its revision (looked up by project and the
    declared hash; none is `422 idempotency_conflict`), a failed one its
    stored failure, an unfinished one `409 conflict`.
  - `receive(…)`, the job body, is public so `ship push` (1.9) can run it
    inside its own operation.
  - `start` removes everything under `<state>/staging` left by a daemon
    that died mid-upload, after `recover` and before serving.
  - `Config.upload_limits: UnpackLimits`; `http::router` gains an
    `Uploads` argument.
- New in `ship-net` (`client.rs`): `RequestBody` (a bounded channel of
  chunks, or bytes) and `client::post(target, path, headers, body,
  timeout)` over both transports, sharing the exchange and the ssh error
  mapping with `get`.
- New dependencies: `ship-source` gains `unicode-normalization` (MIT or
  Apache-2.0); `ship-daemon` gains `ship-source` (allowed: top level);
  `ship` gains dev-dependency `ship-source`. The fuzz crate
  (`crates/ship-source/fuzz`, its own workspace, not a member) uses
  `libfuzzer-sys`.

## Declared file scope

- **owned_paths:**
  - crates/ship-source/
  - crates/ship-daemon/
  - crates/ship-net/src/client.rs
  - crates/ship-net/Cargo.toml
  - crates/ship-core/src/api.rs
  - crates/ship/Cargo.toml
  - crates/ship/tests/ssh_real.rs
  - .github/workflows/ci.yml
  - Cargo.lock
  - .metis/briefs/phase-1-ws-1.5.md
- **read_only_paths:** .metis/adr/, .metis/plans/phase-1-plan.md,
  OVERVIEW.md, docs/contracts/cli-json.md, crates/ship-store/,
  crates/ship-core/src/error.rs, crates/ship-core/src/model.rs,
  crates/ship/tests/fixtures/, deny.toml

## Definition of Done

- [ ] No hostile archive (absolute path, `..`, symlink, hardlink,
      character and block device, FIFO, PAX header, missing parent,
      duplicate, case collision, Unicode NFC/NFD collision, long-name
      traversal) writes a byte outside its staging directory; each is
      rejected with its reason and the staging directory is gone.
- [ ] A decompression bomb is stopped by the expansion-ratio limit with
      expanded bytes bounded by the limit formula, and its staging
      directory removed.
- [ ] Over the compressed, byte or entry limit fails `TooLarge`.
- [ ] A hash mismatch, a truncated stream and a broken transfer each leave
      no revision and no staging directory, and a failed `upload`
      operation with its code.
- [ ] Uploading the same tree twice yields the same revision (`201` then
      `200`); the same tree to another project is another revision.
- [ ] A packed tree unpacks to exactly its files, content and executable
      bits, including long names.
- [ ] The upload works over the local socket (daemon test) and over a
      real sshd (`ssh_real`, CI's `ssh` job).
- [ ] Replaying the idempotency key answers the original result without
      reading the body.
- [ ] A property test over thousands of generated archives and the fuzz
      body over a hostile corpus never write outside the sandbox; CI's
      `fuzz` job runs the cargo-fuzz target for a fixed budget.
- [ ] `metis verify --post` green.

## Test plan

- `ship-source` unit (`unpack.rs`): octal parsing, checksum, path
  validation table, normalisation keys, ratio arithmetic.
- `crates/ship-source/tests/unpack.rs`: round trip of a packed tree;
  each hostile case built with a raw tar writer; limits (compressed,
  bytes, entries, ratio bomb with bound); trailing garbage and non-zero
  trailer; truncated stream; source error is `Transfer`; existing staging
  refused; canary directory beside staging untouched in every case.
- `crates/ship-source/tests/unpack_props.rs`: seeded generator (no new
  dependency) of random archives mixing valid and hostile entries;
  invariants: nothing outside the sandbox changes, failure leaves no
  staging, success yields exactly the valid entries; plus random byte
  mutation of valid archives through `fuzzing::unpack_bytes`.
- `crates/ship-daemon/tests/upload.rs` (real SQLite, local socket):
  201 then 200 for the same tree; another project; replay; missing or bad
  headers 400; digest mismatch, unsafe entry, oversize and truncated body
  leave no revision, no staging, a failed operation; leftover staging
  removed at start; git info appears in the operation log only.
- `crates/ship/tests/ssh_real.rs`: upload over a real sshd yields the same
  revision as over the local socket.
- `ship-net` unit: the POST exchange sends method, headers and body.

## Out-of-scope touches

None.

## Report

- `ship-source/src/unpack.rs` (new): `unpack`, `UnpackLimits`,
  `Unpacked`, `UnpackError`, `UnsafeReason`, `Limit`, `collision_key`,
  `MAX_PATH_BYTES`, `MAX_COMPONENT_BYTES`, `MAX_TRAILER_BYTES`,
  `ZSTD_WINDOW_LOG_MAX`; own tar header parser (checksum, octal fields,
  ustar prefix, GNU `L`), no `tar` crate on this side.
- `ship-source/src/fuzzing.rs` (new, doc-hidden): `check_unpack`,
  `check_unpack_in_temp`, `compress`, `FUZZ_LIMITS`.
- `ship-source/fuzz/` (new, own workspace): cargo-fuzz target `unpack`.
- `ship-source/Cargo.toml`: `unicode-normalization` 0.1. `Cargo.lock`
  adds `unicode-normalization` and `tinyvec` (MIT, Apache-2.0 or Zlib),
  and `ship-source` to ship-daemon's and ship's lists. `cargo deny` was
  not run locally; CI's `audit` job checks it.
- `ship-core/src/api.rs`: `uploads_path`, `SNAPSHOT_SHA256_HEADER`,
  `GIT_COMMIT_HEADER`, `GIT_DIRTY_HEADER`, `UPLOAD_KIND`, `UploadView`.
- `ship-daemon/src/upload.rs` (new): `Uploads`, `UploadRequest`,
  `parse_request`, `receive`, `Received`, `replay`, `Replay`,
  `clean_staging`, `snapshot_dir`, `staging_dir`, `STAGING_DIR`,
  `SNAPSHOTS_DIR`.
- `ship-daemon/src/http.rs`: `POST /api/v1/projects/{project}/uploads`;
  `router` takes `Arc<Uploads>`; the handler waits for the operation's
  terminal phase before answering.
- `ship-daemon/src/queue.rs`: one more `announce` after an operation's
  terminal phase is stored (the terminal event's announcement comes
  before the phase), so a waiter on the phase wakes without polling.
- `ship-daemon/src/lib.rs`: `Config.upload_limits`; `start` runs
  `clean_staging` after `recover`; re-exports `UnpackLimits`.
- `ship-net/src/client.rs`: `RequestBody` (bytes, or a bounded channel),
  `post`; `get` and `post` share `request`, `send`, `over_ssh`,
  `exchange`. The GET wire format is unchanged (empty body).
- `ship/Cargo.toml`: dev-dependency `ship-source`;
  `ship/tests/ssh_real.rs`: `upload_over_real_ssh_matches_local`.
- `.github/workflows/ci.yml`: `fuzz` job (nightly + cargo-fuzz,
  `-max_total_time=300`).
- Tests: `ship-source` unit (6 in `unpack.rs`), `tests/unpack.rs` (12),
  `tests/unpack_props.rs` (2: 2000 generated archives, 2000 corrupted
  ones); `ship-daemon` unit (5 in `upload.rs`), `tests/upload.rs` (6);
  `ship-net` unit (2); `ship-core` unit (1 new, 1 extended).
- `metis verify --post --slice phase-1-ws-1.5`: ALL GREEN (web, fmt,
  clippy, all workspace tests).
- Not run locally: the `fuzz` CI job and `ssh_real` (ignored; CI's
  `ssh` job runs it). This session had no `cargo`/`rustup`, only
  `metis verify`, so the cargo-fuzz target has not been compiled yet;
  CI's `fuzz` job is its first build.
- Decisions:
  - The declared hash travels in a request header, so the client must
    know it before sending (pack once to hash and once to send, or pack
    to a temporary file); that choice belongs to `ship push` (1.9).
  - An upload is its own `upload` operation with no environment, so it
    never waits behind a deploy. `receive` is public for 1.9.
  - The handler waits for the job before answering (`201`/`200` or the
    failure), because the body is read by the job.
  - Snapshot directories are keyed by hash only and shared between
    projects; revisions stay per project (ADR-0007 rule 1).
  - Entry paths with control characters are refused (not required by
    rule 5; keeps logs and terminals safe). File modes become 0644 or
    0755; setuid and similar bits are dropped.
  - Malformed streams and broken transfers end `invalid_request`; size
    and expansion limits `payload_too_large`; refused entries
    `unsafe_snapshot`; a full disk `insufficient_storage`.
  - Collisions use NFD → lowercase → NFC; HFS+ ignorable code points
    are not folded.
  - No idle timeout on an upload's body: a stalled client holds one
    queue slot until it disconnects.
