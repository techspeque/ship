---
type: adr
id: 0009
title: Config values and write-only secrets exist from the first release; encryption at rest follows
status: proposed
date: 2026-11-24
phase: 1
---

# ADR-0009: Config values and write-only secrets exist from the first release; encryption at rest follows

> Phase: 1 | Status: proposed | Date: 2026-11-24
> Decision drivers: snapshots exclude `.env` (ADR-0005), and almost every real app needs environment values to start.

## Context

The v0.4 draft excluded `.env` from pushes in its first milestone and
delivered variables and secrets in its second. A first release that cannot
give an app its database URL cannot host real projects. But a full
encrypted store with pluggable backends is too much for Phase 1.

## Decision

Phase 1 ships plain config values and write-only secrets with file
permissions as the protection at rest, and says so. Phase 2 adds encryption
at rest behind a backend interface shaped like Gaia's (Gaia ADR-0007).

### Rules this decision implies:

1. Config values are non-secret, readable through the API, scoped to project, environment or service. Precedence, lowest first: manifest defaults, project, environment, service.
2. Secrets are set through `--prompt`, `--stdin` or the UI, never as arguments. The API returns names and timestamps, never values.
3. Phase 1 stores secret values in files under the state directory, mode 0600, owned by the `ship` user. The docs state that this is not encryption.
4. Phase 2 encrypts the built-in store with a key kept outside the database, and defines a backend interface. The built-in store is the only backend in v1.
5. At deploy, values are written to a permission-restricted env file outside the snapshot, passed to Compose, and never baked into an image, a build argument or a saved Compose file.
6. Changing a value does nothing until a redeploy or restart; Ship shows which services are running with stale values.
7. The logger redacts every known secret value. An invariant test greps all outputs for test secrets.
8. Copying an environment's configuration copies config values and secret names, never secret values.
9. The docs state that a service can read its own secrets and that anyone with Docker access can read all of them.

## Consequences

### Positive
- Phase 1 can host apps that need credentials.
- Secrets never travel in a snapshot or sit in shell history.
- The Phase 2 store replaces the storage, not the interface.

### Negative
- Phase 1 secrets are plaintext on the host's disk.
- A migration of stored secrets is needed at Phase 2.

### Neutral
- Ship's store and Gaia's store stay separate (ADR-0004).

## Alternatives Considered

| Option | Pros | Cons | Why rejected |
|---|---|---|---|
| No config until Phase 2 (v0.4) | Smaller Phase 1 | Real apps cannot start | Usability |
| Upload `.env` with the snapshot | Zero setup | Secrets in every revision | Invariant |
| Encrypted store in Phase 1 | No migration | Key management delays the first release | Scope |

## References

- ADR-0004, ADR-0005, ADR-0008
- Gaia ADR-0007
