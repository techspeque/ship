---
type: adr
id: 0005
title: A push is a streamed tar+zstd snapshot of the working tree, unpacked defensively
status: proposed
date: 2026-11-24
phase: 1
---

# ADR-0005: A push is a streamed tar+zstd snapshot of the working tree, unpacked defensively

> Phase: 1 | Status: proposed | Date: 2026-11-24
> Decision drivers: goal 1, one command from working tree to running app, without a Git round-trip.

## Context

`ship push .` must send what is on disk now, committed or not, from a
directory that may not be a Git repository at all. The archive is the one
input an attacker or an accident can shape freely, so unpacking it is the
most dangerous code in Phase 1. The v0.4 draft used ZIP, whose directory
sits at the end of the file, so it cannot be checked while it streams.

## Decision

The client walks the directory, applies ignore rules, and streams a
tar archive compressed with zstd to the daemon with a SHA-256 of the stream.
The daemon validates every entry as it arrives, unpacks into a new staging
directory and promotes it to an immutable revision only when the whole
stream has passed.

### Rules this decision implies:

1. Included: tracked, untracked and modified files not ignored by `.gitignore` or `.shipignore`. No Git metadata is required.
2. Always excluded unless named explicitly: `.git/`, `.env`, `.env.*` except template files such as `.env.example`, private keys, credential stores, and build directories such as `node_modules/` and `target/`.
3. `.dockerignore` is sent as a file; Docker applies it during the build.
4. `--dry-run` lists file count, size and every sensitive file excluded. The client refuses a home directory or a tree over the size limit.
5. The daemon rejects absolute paths, `..`, symlinks, hardlinks, devices, FIFOs, paths that collide after Unicode or case normalisation, and anything over the size, file-count or expansion-ratio limits.
6. The client does not follow symlinks while packing; it reports them as skipped.
7. A stream that fails any check leaves nothing behind but a failed operation record.
8. A revision is identified by the stream hash. A local Git commit and dirty flag are recorded as information only.
9. Upload and unpack use bounded memory and are not resumable; a broken transfer is retried from the start.

## Consequences

### Positive
- Deploys exactly what is on disk, from any directory.
- Validation happens before any byte is trusted.
- Secrets in `.env` files do not leave the laptop by accident.

### Negative
- Projects that rely on symlinks in the build context fail until a later phase allows in-tree links.
- Every push uploads the whole tree; no delta transfer.
- Apps that read `.env` at runtime need their values set through Ship (ADR-0009).

### Neutral
- Build caching is Docker's; an unchanged tree still rebuilds quickly through layer cache.

## Alternatives Considered

| Option | Pros | Cons | Why rejected |
|---|---|---|---|
| ZIP (v0.4) | Familiar | Cannot validate while streaming | Unpack safety |
| rsync over ssh | Deltas | Second transport, no single hash, ssh-only | One path for all transports |
| Build locally, push image | No source on host | Needs a registry, cross-arch builds | Breaks one command |
| `git push` to the host | Standard | Excludes uncommitted work | Breaks goal 1 |

## References

- ADR-0007, ADR-0009, ADR-0011
