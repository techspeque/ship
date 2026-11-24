---
type: adr
id: 0001
title: One host; Docker Engine and Compose v2 are the runtime
status: proposed
date: 2026-11-24
phase: 0
---

# ADR-0001: One host; Docker Engine and Compose v2 are the runtime

> Phase: 0 | Status: proposed | Date: 2026-11-24
> Decision drivers: goal 2, operate applications without a cluster; keep the thing small enough for one person to build and run.

## Context

The projects Ship must host first already have Dockerfiles or Compose files
and run fine on one machine. Every tool that grows a scheduler stops being
small. Docker already owns building, networking, volumes, restart policies
and health checks, and Compose already describes multi-service applications.

## Decision

A Ship installation manages exactly one Docker Engine on the machine the
daemon runs on. Ship drives it by running `docker` and `docker compose` as
subprocesses. Every environment is one Compose project.

### Rules this decision implies:

1. No cluster membership, scheduling, replication or cross-host state. A client may know several targets; they are independent installations.
2. All Docker access goes through `ship-docker`, with structured argv, no shell and no user-supplied flags.
3. A Dockerfile source becomes a generated one-service Compose project, so there is a single deploy path.
4. Compose project names derive from the immutable environment id, never from user input.
5. Ship labels every object it creates and touches nothing without its labels unless explicitly adopted.
6. Preflight checks the Docker and Compose versions against a range validated in CI and refuses to run outside it.
7. Podman and rootless Docker are unsupported until tested.

## Consequences

### Positive
- Existing Dockerfiles and Compose files work with little or no change.
- Apps keep running when the Ship daemon is down.
- Nothing to learn that Docker users do not already know.

### Negative
- Ship inherits Compose's behaviour and bugs, and must track its CLI output across versions.
- No path to a second host without a different product.
- Subprocess output parsing is less precise than an API client.

### Neutral
- The daemon needs Docker access, which is root-equivalent (see Security Model).

## Alternatives Considered

| Option | Pros | Cons | Why rejected |
|---|---|---|---|
| Docker Engine API directly (bollard) | Typed, no parsing | Reimplements Compose semantics | Compose compatibility is the point |
| Swarm / Nomad / k3s | Multi-host later | Operational weight now | Breaks the purpose |
| Own container supervision | Full control | Years of work | Not a small tool |

## References

- ADR-0006, ADR-0008, ADR-0011
