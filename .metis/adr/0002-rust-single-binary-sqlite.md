---
type: adr
id: 0002
title: One Rust binary for client and daemon; SQLite for state; UI embedded
status: proposed
date: 2026-11-24
phase: 0
---

# ADR-0002: One Rust binary for client and daemon; SQLite for state; UI embedded

> Phase: 0 | Status: proposed | Date: 2026-11-24
> Decision drivers: easy install and uninstall; no service Ship depends on besides Docker; the maintainer's choice of language.

## Context

Gaia and Metis are Go. Writing Ship in Go would allow shared packages,
embedded `tsnet` and native Compose parsing. The maintainer has chosen Rust
for Ship regardless. The cost of that choice is paid in ADR-0003 (no embedded
tailnet) and ADR-0004 (alignment through contracts, not code).

A PaaS that needs its own Postgres or message broker is no longer
lightweight. Two binaries double the install and version-skew problems.

## Decision

Ship is one Rust binary named `ship`. `ship daemon` runs the server; every
other subcommand is the client. State is one SQLite database plus files in
one state directory. The Svelte UI is compiled to static files and embedded.

### Rules this decision implies:

1. Stack: Axum, Tokio, SQLx (SQLite, WAL), Clap. No external database, broker or cache.
2. `ship daemon` is supported on Linux. Client subcommands are supported on Linux and macOS, amd64 and arm64.
3. Schema changes only through in-repo migrations; domain mutations in transactions; Docker calls are never inside one.
4. The workspace follows the crate layout and dependency rule in OVERVIEW §2.3.
5. No Node.js at runtime. The UI build is reproducible and its output version-locked to the binary.
6. No plugin system and no second process until a second real use needs one.

## Consequences

### Positive
- Install is one file; backup is one directory.
- Client and daemon share types, so the API cannot drift between them.
- Small idle footprint.

### Negative
- No code shared with Gaia or Metis; conventions are copied by hand.
- No embedded tailnet client.
- macOS builds carry daemon code they cannot use.

### Neutral
- SQLite limits Ship to one writer process, which matches one daemon per host.

## Alternatives Considered

| Option | Pros | Cons | Why rejected |
|---|---|---|---|
| Go, sharing packages with Gaia | tsnet, compose-go, shared code | Not the maintainer's choice | Decided |
| Separate `ship` and `ship-server` binaries | Smaller client | Two artifacts, version skew | Install simplicity |
| Postgres for state | Concurrency | A service to run | Breaks lightweight |

## References

- ADR-0003, ADR-0004
- Gaia ADR-0002, ADR-0006
